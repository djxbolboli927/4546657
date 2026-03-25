use anyhow::{Context, Result};
use solana_sdk::{signature::Keypair, signer::Signer, transaction::VersionedTransaction};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing::{info, warn};

pub mod proto {
    pub mod auth {
        tonic::include_proto!("auth");
    }
    pub mod bundle {
        tonic::include_proto!("bundle");
    }
    pub mod searcher {
        tonic::include_proto!("searcher");
    }
    pub mod packet {
        tonic::include_proto!("packet");
    }
    #[allow(dead_code)]
    pub mod shared {
        tonic::include_proto!("shared");
    }
}

use proto::auth::auth_service_client::AuthServiceClient;
use proto::auth::{GenerateAuthChallengeRequest, GenerateAuthTokensRequest, Role};
use proto::searcher::searcher_service_client::SearcherServiceClient;
use proto::searcher::SendBundleRequest;

type AuthenticatedSearcherClient =
    SearcherServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>;

/// Jito gRPC client that handles authentication and bundle submission.
/// Uses the official Jito mev-protos: SearcherService for sending bundles.
/// Auth uses whitelisted keypair (not trading wallet).
pub struct JitoClient {
    searcher_client: AuthenticatedSearcherClient,
    _auth_keypair: Keypair,
}

impl JitoClient {
    /// Connect to a Jito block engine gRPC endpoint and authenticate.
    pub async fn connect(grpc_url: &str, auth_keypair: Keypair) -> Result<Self> {
        let tls_config = ClientTlsConfig::new().with_webpki_roots();

        let channel = Endpoint::from_shared(grpc_url.to_string())?
            .tls_config(tls_config)?
            .connect()
            .await
            .context("failed to connect to Jito gRPC")?;

        // Authenticate with whitelisted keypair
        let access_token = Self::authenticate(&channel, &auth_keypair).await?;
        info!("Jito gRPC auth successful");

        // Create authenticated searcher client
        let searcher_client = SearcherServiceClient::with_interceptor(
            channel,
            AuthInterceptor::new(access_token),
        );

        Ok(Self {
            searcher_client,
            _auth_keypair: auth_keypair,
        })
    }

    /// Authenticate per official Jito proto:
    /// 1. Send pubkey as raw 32 bytes + Role::Searcher
    /// 2. Sign: the challenge is signed with the private key
    ///    The signed message is: pubkey_bytes + challenge_bytes
    /// 3. Send signed_challenge as 64-byte signature
    async fn authenticate(channel: &Channel, keypair: &Keypair) -> Result<String> {
        let mut auth_client = AuthServiceClient::new(channel.clone());

        let pubkey_bytes = keypair.pubkey().to_bytes().to_vec();

        // Step 1: Request challenge — pubkey as raw 32 bytes, role = SEARCHER (1)
        let challenge_resp = auth_client
            .generate_auth_challenge(GenerateAuthChallengeRequest {
                role: Role::Searcher as i32,
                pubkey: pubkey_bytes.clone(),
            })
            .await
            .context("auth challenge request failed")?;

        let challenge = challenge_resp.into_inner().challenge;

        // Step 2: Sign: prepend pubkey to challenge, then sign
        // Per Jito proto docs: "sign(pubkey, challenge)"
        let mut sign_data = Vec::with_capacity(32 + challenge.len());
        sign_data.extend_from_slice(&pubkey_bytes);
        sign_data.extend_from_slice(challenge.as_bytes());
        let signature = keypair.sign_message(&sign_data);

        // Step 3: Send signed challenge — client_pubkey as raw 32 bytes
        let tokens_resp = auth_client
            .generate_auth_tokens(GenerateAuthTokensRequest {
                challenge,
                client_pubkey: pubkey_bytes,
                signed_challenge: signature.as_ref().to_vec(),
            })
            .await
            .context("auth tokens request failed")?;

        let tokens = tokens_resp.into_inner();
        let access_token = tokens
            .access_token
            .ok_or_else(|| anyhow::anyhow!("no access token returned"))?
            .value;

        Ok(access_token)
    }

    /// Send a single-transaction bundle via SearcherService.SendBundle.
    /// Per Jito: bundle can contain up to 5 txs, tip must be in last tx.
    /// We use single-tx bundles for maximum speed.
    pub async fn send_bundle(&mut self, tx: &VersionedTransaction) -> Result<String> {
        let tx_bytes = bincode::serialize(tx).context("failed to serialize transaction")?;

        let packet = proto::packet::Packet {
            data: tx_bytes.clone(),
            meta: Some(proto::packet::Meta {
                size: tx_bytes.len() as u64,
                addr: String::new(),
                port: 0,
                flags: None,
                sender_stake: 0,
            }),
        };

        let bundle = proto::bundle::Bundle {
            header: None,
            packets: vec![packet],
        };

        let resp = self
            .searcher_client
            .send_bundle(SendBundleRequest {
                bundle: Some(bundle),
            })
            .await
            .context("send_bundle gRPC failed")?;

        let uuid = resp.into_inner().uuid;
        Ok(uuid)
    }
}

/// gRPC interceptor that attaches the Bearer access token to every request.
#[derive(Clone)]
struct AuthInterceptor {
    access_token: String,
}

impl AuthInterceptor {
    fn new(token: String) -> Self {
        Self {
            access_token: token,
        }
    }
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> std::result::Result<tonic::Request<()>, tonic::Status> {
        let value = format!("Bearer {}", self.access_token)
            .parse()
            .map_err(|_| tonic::Status::internal("invalid access token"))?;
        request
            .metadata_mut()
            .insert("authorization", value);
        Ok(request)
    }
}
