//! Capability probe only. No real balances, trading commands or secrets are
//! accepted. A real worker must add encrypted commands and finalized root checks.
use outlayer::{encryption_keys, signing_keys, storage::sealed};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

fn probe() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let input: serde_json::Value = {
        let mut text = String::new();
        std::io::stdin().take(4097).read_to_string(&mut text)?;
        if text.len() > 4096 {
            return Err("input too large".into());
        }
        serde_json::from_str(&text)?
    };
    if input.get("op").and_then(|v| v.as_str()) != Some("probe")
        || input.as_object().is_none_or(|o| o.len() != 1)
    {
        return Err("only a nonfinancial capability probe is supported".into());
    }
    let aad = b"optionality-confidential-probe-v1";
    let sealed_bytes = encryption_keys::encrypt("ledger", None, b"nonfinancial-test-marker", aad)
        .map_err(|e| format!("encryption initialization: {e}"))?;
    if encryption_keys::decrypt("ledger", None, &sealed_bytes, aad)? != b"nonfinancial-test-marker"
    {
        return Err("encryption roundtrip failed".into());
    }
    let count = match sealed::get("ledger", None, "probe-counter")? {
        Some(previous) => {
            let count: u64 = serde_json::from_slice(previous.plaintext())?;
            let next = count.checked_add(1).ok_or("counter overflow")?;
            let (updated, _) = sealed::set_if_equals(
                "ledger",
                None,
                "probe-counter",
                &previous,
                &serde_json::to_vec(&next)?,
            )?;
            if !updated {
                return Err("concurrent probe; retry".into());
            }
            next
        }
        None => {
            if !sealed::set_if_absent("ledger", None, "probe-counter", b"1")? {
                return Err("concurrent initialization; retry".into());
            }
            1
        }
    };
    let public = signing_keys::public_key("receipt", None)?;
    let digest = Sha256::digest([aad.as_slice(), &count.to_le_bytes()].concat());
    let signature = signing_keys::sign("receipt", None, &digest)?;
    Ok(
        json!({"probe_only":true,"sealed_storage":true,"counter":count,"digest":hex::encode(digest),"public_key":hex::encode(public),"signature":hex::encode(signature)}),
    )
}
fn main() {
    let result = match probe() {
        Ok(result) => result,
        Err(_) => {
            json!({"probe_only":true,"ok":false,"error":"capability probe refused; inspect configuration"})
        }
    };
    let bytes = serde_json::to_vec(&result).expect("small JSON output");
    std::io::stdout().write_all(&bytes).expect("stdout");
    std::io::stdout().flush().expect("flush");
}
