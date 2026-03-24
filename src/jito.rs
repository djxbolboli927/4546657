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
}

use proto::auth::auth_service_client::AuthServiceClient;
use proto::auth::role::RoleType;
use proto::auth::{GenerateAuthChallengeRequest, GenerateAuthTokensRequest};
use proto::bundle::bundle_service_client::BundleServiceClient;
use proto::bundle::{Bundle, GetBundleStatusesRequest, Packet, SendBundleRequest};

/// Jito gRPC client that handles authentication and bundle submission.
pub struct JitoClient {
    bundle_client: BundleServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
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

        // Authenticate
        let access_token = Self::authenticate(&channel, &auth_keypair).await?;
        info!("Jito auth successful");

        // Create authenticated bundle client
        let bundle_client = BundleServiceClient::with_interceptor(
            channel,
            AuthInterceptor::new(access_token),
        );

        Ok(Self {
            bundle_client,
            _auth_keypair: auth_keypair,
        })
    }

    async fn authenticate(channel: &Channel, keypair: &Keypair) -> Result<String> {
        let mut auth_client = AuthServiceClient::new(channel.clone());

        // Step 1: Request challenge
        let challenge_resp = auth_client
            .generate_auth_challenge(GenerateAuthChallengeRequest {
                role: RoleType::Searcher as i32,
                pubkey: keypair.pubkey().to_string(),
            })
            .await
            .context("auth challenge request failed")?;

        let challenge = challenge_resp.into_inner().challenge;

        // Step 2: Sign challenge and get tokens
        let signature = keypair.sign_message(challenge.as_bytes());

        let tokens_resp = auth_client
            .generate_auth_tokens(GenerateAuthTokensRequest {
                challenge,
                client_pubkey: keypair.pubkey().to_string(),
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

    /// Send a single-transaction bundle to Jito.
    pub async fn send_bundle(&mut self, tx: &VersionedTransaction) -> Result<String> {
        let tx_bytes = bincode::serialize(tx).context("failed to serialize transaction")?;

        let packet = Packet {
            data: tx_bytes.clone(),
            meta: Some(proto::bundle::Meta {
                size: tx_bytes.len() as u64,
                addr: String::new(),
                port: 0,
                flags: 0,
                sender_stake: 0,
            }),
        };

        let bundle = Bundle {
            packets: vec![packet],
        };

        let resp = self
            .bundle_client
            .send_bundle(SendBundleRequest {
                bundle: Some(bundle),
            })
            .await
            .context("send_bundle gRPC failed")?;

        let uuid = resp.into_inner().uuid;
        info!(uuid = %uuid, "bundle submitted");
        Ok(uuid)
    }

    /// Check bundle status by UUID.
    pub async fn get_bundle_status(&mut self, uuid: &str) -> Result<Option<String>> {
        let resp = self
            .bundle_client
            .get_bundle_statuses(GetBundleStatusesRequest {
                bundle_ids: vec![uuid.to_string()],
            })
            .await;

        match resp {
            Ok(r) => {
                let statuses = r.into_inner().statuses;
                if let Some(status) = statuses.first() {
                    Ok(Some(status.status.clone()))
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                warn!(error = %e, "get_bundle_statuses failed");
                Ok(None)
            }
        }
    }
}

/// gRPC interceptor that attaches the access token to every request.
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
