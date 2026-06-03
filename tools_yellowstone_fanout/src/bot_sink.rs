use crate::config::{Config, OutputMode};

/// Phase A "bot sink": emit a decoded account update.
///
/// Default (stdout) prints ONE compact summary line per update — never the full
/// data blob. Pass `--dump-json` only for deep debugging; it base64-encodes the
/// entire account data, which is heavy and must not be used under load.
#[allow(clippy::too_many_arguments)]
pub fn emit_account(
    cfg: &Config,
    slot: u64,
    pubkey: &str,
    owner: &str,
    lamports: u64,
    data: &[u8],
    write_version: u64,
    is_startup: bool,
) {
    if cfg.output_mode == OutputMode::None {
        return;
    }

    if cfg.dump_json {
        let data_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
        println!(
            "{{\"kind\":\"account\",\"slot\":{slot},\"pubkey\":\"{pubkey}\",\"owner\":\"{owner}\",\"lamports\":{lamports},\"write_version\":{write_version},\"is_startup\":{is_startup},\"data_len\":{},\"data_base64\":\"{data_b64}\"}}",
            data.len()
        );
    } else {
        println!(
            "slot={slot} pubkey={pubkey} owner={owner} lamports={lamports} data_len={} write_version={write_version} startup={is_startup}",
            data.len()
        );
    }
}
