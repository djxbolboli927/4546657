//! Jito Block Engine gRPC searcher client.
//!
//! We don't "connect" to Jito in any stateful sense -- the block engine is a
//! plain gRPC server. What the grpc.json keypair *does* is prove to Jito that
//! we're a registered searcher by signing a one-shot challenge (see
//! https://jito-foundation.gitbook.io/mev/). The flow, every time tokens are
//! near expiry:
//!
//!   1. AuthService.GenerateAuthChallenge(role=Searcher, pubkey) -> challenge
//!   2. sign message = "{pubkey_base58}-{challenge}" with grpc.json keypair
//!   3. AuthService.GenerateAuthTokens(challenge, pubkey, signature)
//!         -> access_token + refresh_token
//!   4. Every SearcherService RPC we make attaches
//!         `authorization: Bearer {access_token}` as gRPC metadata.
//!
//! A background task refreshes the access token ~60s before it expires (and
//! re-runs the full challenge flow if the refresh token itself is about to
//! expire). There is NO long-lived stream -- if refresh is failing we just
//! keep trying, but the hot `send_bundle` path never blocks on auth: it grabs
//! the current token from an RwLock and posts the bundle immediately.

use anyhow::{anyhow, Context, Result};
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tonic::{
    metadata::MetadataValue,
    transport::{Channel, ClientTlsConfig, Endpoint},
    Request,
};
use tracing::{debug, info, warn};

pub mod proto {
    pub mod auth {
        tonic::include_proto!("auth");
    }
    pub mod packet {
        tonic::include_proto!("packet");
    }
    pub mod shared {
        tonic::include_proto!("shared");
    }
    pub mod bundle {
        tonic::include_proto!("bundle");
    }
    pub mod searcher {
        tonic::include_proto!("searcher");
    }
}

use proto::auth::auth_service_client::AuthServiceClient;
use proto::auth::{
    GenerateAuthChallengeRequest, GenerateAuthTokensRequest, RefreshAccessTokenRequest, Role, Token,
};
use proto::bundle::Bundle;
use proto::packet::{Meta, Packet, PacketFlags};
use proto::searcher::searcher_service_client::SearcherServiceClient;
use proto::searcher::SendBundleRequest;

/// Refresh the access token when it has this many seconds of life left.
const REFRESH_SAFETY_MARGIN_SECS: i64 = 60;
/// If the refresh token has less than this many seconds left, re-run the full
/// challenge flow instead of calling RefreshAccessToken.
const REAUTH_SAFETY_MARGIN_SECS: i64 = 120;

#[derive(Clone)]
struct TokenHolder {
    access: Arc<RwLock<String>>,
}

impl TokenHolder {
    fn new() -> Self {
        Self {
            access: Arc::new(RwLock::new(String::new())),
        }
    }

    fn set(&self, val: String) {
        *self.access.write().unwrap() = val;
    }

    fn get(&self) -> String {
        self.access.read().unwrap().clone()
    }
}

/// A gRPC channel + live auth token against one Jito block engine endpoint.
#[derive(Clone)]
pub struct JitoGrpcEndpoint {
    channel: Channel,
    token: TokenHolder,
    url: String,
}

impl JitoGrpcEndpoint {
    /// Connect, run the full auth challenge, and spawn the refresh task.
    /// Returns as soon as the first access token is in hand.
    pub async fn connect(url: &str, keypair: Arc<Keypair>) -> Result<Self> {
        let endpoint = build_endpoint(url)?;
        let channel = endpoint
            .connect()
            .await
            .with_context(|| format!("Jito gRPC dial failed for {}", url))?;

        let token = TokenHolder::new();
        let (access, refresh) = authenticate(channel.clone(), keypair.as_ref()).await?;
        token.set(access.value.clone());

        let access_exp = token_expiry_secs(&access);
        let refresh_exp = token_expiry_secs(&refresh);
        info!(
            endpoint = url,
            access_expires_in = access_exp.map(|t| t - now_secs()).unwrap_or(-1),
            refresh_expires_in = refresh_exp.map(|t| t - now_secs()).unwrap_or(-1),
            "Jito gRPC authenticated"
        );

        // Background refresh: never blocks the hot path.
        spawn_refresh_loop(
            url.to_string(),
            channel.clone(),
            keypair,
            token.clone(),
            access,
            refresh,
        );

        Ok(Self {
            channel,
            token,
            url: url.to_string(),
        })
    }

    /// Send ONE versioned transaction as a single-tx Bundle to this endpoint.
    /// Returns the bundle UUID on success.
    ///
    /// This path makes ZERO blocking calls: token is read from an RwLock, the
    /// tx bytes are serialised once by the caller, and the gRPC unary RPC is
    /// the only I/O.
    pub async fn send_bundle(&self, tx_bytes: &[u8]) -> Result<String> {
        let tok = self.token.get();
        if tok.is_empty() {
            anyhow::bail!("no Jito gRPC access token yet ({})", self.url);
        }
        let auth: MetadataValue<_> = format!("Bearer {}", tok)
            .parse()
            .map_err(|e| anyhow!("bad auth token: {e}"))?;

        let bundle = Bundle {
            header: None,
            packets: vec![Packet {
                data: tx_bytes.to_vec(),
                meta: Some(Meta {
                    size: tx_bytes.len() as u64,
                    addr: String::new(),
                    port: 0,
                    flags: Some(PacketFlags::default()),
                    sender_stake: 0,
                }),
            }],
        };

        let mut req = Request::new(SendBundleRequest {
            bundle: Some(bundle),
        });
        req.metadata_mut().insert("authorization", auth);

        let mut client = SearcherServiceClient::new(self.channel.clone());
        let resp = client
            .send_bundle(req)
            .await
            .with_context(|| format!("Jito gRPC SendBundle failed at {}", self.url))?;
        Ok(resp.into_inner().uuid)
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Fan-out wrapper: one authenticated channel per endpoint. `send_bundle`
/// broadcasts to ALL endpoints concurrently and returns the first UUID.
pub struct JitoGrpcMulti {
    endpoints: Vec<JitoGrpcEndpoint>,
}

impl JitoGrpcMulti {
    pub async fn connect(urls: &[String], keypair: Arc<Keypair>) -> Result<Self> {
        if urls.is_empty() {
            anyhow::bail!("jito_grpc.endpoints is empty");
        }
        let mut endpoints = Vec::with_capacity(urls.len());
        for url in urls {
            match JitoGrpcEndpoint::connect(url, keypair.clone()).await {
                Ok(ep) => endpoints.push(ep),
                Err(e) => warn!(
                    endpoint = %url,
                    error = %e,
                    "Jito gRPC endpoint auth failed, skipping",
                ),
            }
        }
        if endpoints.is_empty() {
            anyhow::bail!("no Jito gRPC endpoints came up");
        }
        info!(
            endpoints = endpoints.len(),
            "Jito gRPC multi-region client ready"
        );
        Ok(Self { endpoints })
    }

    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Broadcast a single-tx bundle to every endpoint. Returns the first UUID
    /// we get back; logs the slower ones as they arrive.
    #[allow(dead_code)]
    pub async fn send_bundle(&self, tx: &VersionedTransaction) -> Result<String> {
        let tx_bytes = bincode::serialize(tx).context("bincode tx")?;
        self.send_bundle_bytes(&tx_bytes).await
    }

    /// Same as `send_bundle` but the caller has already serialized the tx
    /// (the hot path does this once and shares the bytes with the REST send).
    pub async fn send_bundle_bytes(&self, tx_bytes: &[u8]) -> Result<String> {
        let tx_bytes = Arc::new(tx_bytes.to_vec());

        let mut futs = futures::stream::FuturesUnordered::new();
        for ep in &self.endpoints {
            let ep = ep.clone();
            let tx_bytes = tx_bytes.clone();
            futs.push(async move {
                let url = ep.url().to_string();
                let res = ep.send_bundle(&tx_bytes).await;
                (url, res)
            });
        }

        use futures::StreamExt;
        let mut first_ok: Option<String> = None;
        let mut last_err: Option<anyhow::Error> = None;
        while let Some((url, res)) = futs.next().await {
            match res {
                Ok(uuid) => {
                    debug!(endpoint = %url, bundle_id = %uuid, "jito_grpc bundle accepted");
                    if first_ok.is_none() {
                        first_ok = Some(uuid);
                    }
                }
                Err(e) => {
                    debug!(endpoint = %url, error = %e, "jito_grpc bundle rejected");
                    last_err = Some(e);
                }
            }
        }

        first_ok.ok_or_else(|| last_err.unwrap_or_else(|| anyhow!("no jito_grpc endpoints responded")))
    }
}

fn build_endpoint(url: &str) -> Result<Endpoint> {
    let endpoint = Endpoint::from_shared(url.to_string())
        .with_context(|| format!("invalid jito_grpc url: {}", url))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .context("tls config")?
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .connect_timeout(Duration::from_secs(5));
    Ok(endpoint)
}

/// Run the 3-step challenge/sign/token-exchange flow against the given channel.
async fn authenticate(channel: Channel, keypair: &Keypair) -> Result<(Token, Token)> {
    let mut auth = AuthServiceClient::new(channel);
    let pubkey = keypair.pubkey();
    let pubkey_bytes = pubkey.to_bytes().to_vec();

    let challenge_resp = auth
        .generate_auth_challenge(GenerateAuthChallengeRequest {
            role: Role::Searcher as i32,
            pubkey: pubkey_bytes.clone(),
        })
        .await
        .context("GenerateAuthChallenge failed (check grpc.json is whitelisted)")?
        .into_inner();

    // Jito contract: the bytes to sign are the literal string
    //    "{pubkey_base58}-{challenge}"
    // This is what the reference jito-labs/searcher-examples client does.
    let to_sign = format!("{}-{}", pubkey, challenge_resp.challenge).into_bytes();
    let signature = keypair.sign_message(&to_sign).as_ref().to_vec();

    let tokens = auth
        .generate_auth_tokens(GenerateAuthTokensRequest {
            challenge: challenge_resp.challenge,
            client_pubkey: pubkey_bytes,
            signed_challenge: signature,
        })
        .await
        .context("GenerateAuthTokens failed (bad signature?)")?
        .into_inner();

    let access = tokens
        .access_token
        .ok_or_else(|| anyhow!("auth response missing access_token"))?;
    let refresh = tokens
        .refresh_token
        .ok_or_else(|| anyhow!("auth response missing refresh_token"))?;
    Ok((access, refresh))
}

fn spawn_refresh_loop(
    url: String,
    channel: Channel,
    keypair: Arc<Keypair>,
    holder: TokenHolder,
    initial_access: Token,
    initial_refresh: Token,
) {
    tokio::spawn(async move {
        let mut access = initial_access;
        let mut refresh = initial_refresh;
        loop {
            let now = now_secs();
            let access_exp = token_expiry_secs(&access).unwrap_or(now);
            let refresh_exp = token_expiry_secs(&refresh).unwrap_or(now);

            let sleep_secs = (access_exp - now - REFRESH_SAFETY_MARGIN_SECS).max(5);
            tokio::time::sleep(Duration::from_secs(sleep_secs as u64)).await;

            let now = now_secs();
            if refresh_exp - now <= REAUTH_SAFETY_MARGIN_SECS {
                // Refresh token near expiry -> full re-auth.
                match authenticate(channel.clone(), keypair.as_ref()).await {
                    Ok((a, r)) => {
                        holder.set(a.value.clone());
                        access = a;
                        refresh = r;
                        info!(endpoint = %url, "Jito gRPC re-authenticated");
                    }
                    Err(e) => {
                        warn!(endpoint = %url, error = %e, "Jito gRPC re-auth failed; will retry");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                }
                continue;
            }

            // Normal path: refresh the access token.
            let mut auth = AuthServiceClient::new(channel.clone());
            match auth
                .refresh_access_token(RefreshAccessTokenRequest {
                    refresh_token: refresh.value.clone(),
                })
                .await
            {
                Ok(resp) => {
                    if let Some(a) = resp.into_inner().access_token {
                        holder.set(a.value.clone());
                        access = a;
                        debug!(endpoint = %url, "Jito gRPC access token refreshed");
                    }
                }
                Err(e) => {
                    warn!(endpoint = %url, error = %e, "Jito gRPC refresh failed; will full re-auth");
                    match authenticate(channel.clone(), keypair.as_ref()).await {
                        Ok((a, r)) => {
                            holder.set(a.value.clone());
                            access = a;
                            refresh = r;
                            info!(endpoint = %url, "Jito gRPC re-authenticated (after refresh fail)");
                        }
                        Err(e) => {
                            warn!(endpoint = %url, error = %e, "Jito gRPC re-auth also failed");
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                    }
                }
            }
        }
    });
}

fn token_expiry_secs(token: &Token) -> Option<i64> {
    token.expires_at_utc.as_ref().map(|ts| ts.seconds)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
