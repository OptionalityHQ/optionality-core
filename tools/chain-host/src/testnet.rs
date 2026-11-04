//! Operate existing public-testnet contracts. Never deploys or initializes them.
use super::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Children(Vec<std::process::Child>);
impl Drop for Children {
    fn drop(&mut self) { for child in &mut self.0 { let _ = child.kill(); let _ = child.wait(); } }
}

pub async fn run(path: &str) -> Result<()> {
    let session: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    anyhow::ensure!(session["network"] == "testnet", "operator only supports testnet metadata");
    let rpc = session["rpc"].as_str().context("RPC missing")?;
    let worker = near_workspaces::testnet().rpc_addr(rpc).await?;
    let credential = optional("--credentials").context("pass --credentials with the venue account's local credential file")?;
    let credential_path = PathBuf::from(&credential);
    let key: serde_json::Value = serde_json::from_slice(&fs::read(&credential_path)?)?;
    let account_id = key["account_id"].as_str().or_else(|| credential_path.file_stem().and_then(|s| s.to_str())).context("credential account missing")?;
    let private_key = key["private_key"].as_str().or_else(|| key["secret_key"].as_str()).context("credential private key missing")?;
    let admin = Account::from_secret_key(account_id.parse()?, private_key.parse()?, &worker);
    anyhow::ensure!(Some(admin.id().as_str()) == session["admin"]["account"].as_str(), "credential does not match deployment admin");
    let options = Contract::from_secret_key(session["contracts"]["options"].as_str().context("options missing")?.parse()?, admin.secret_key().clone(), &worker);
    let oracle = Contract::from_secret_key(session["contracts"]["oracle"].as_str().context("oracle missing")?.parse()?, admin.secret_key().clone(), &worker);
    let engine = optional("--engine").context("pass --engine with the live pricing engine URL")?;
    let keys = PathBuf::from(arg("--seat-keys", "data/testnet-seats"));
    // Refuse missing identities: public-chain seats cannot be regenerated on restart.
    for name in oracle::SEAT_NAMES { anyhow::ensure!(keys.join(format!("{name}.key")).is_file(), "missing durable key for {name}"); }
    let (seats, children) = oracle::start_committee_with(Path::new("data/testnet-sockets"), &engine, Some(&keys)).map_err(anyhow::Error::msg)?;
    let _children = Children(children);
    let sources: Vec<serde_json::Value> = oracle.view("get_sources").await?.json()?;
    for seat in &seats {
        anyhow::ensure!(sources.iter().any(|source| source["name"] == seat.name && source["public_key"] == json!(seat.public_key) && source["removed_ns"] == 0), "local identity for {} differs from registered oracle seat", seat.name);
    }
    println!("testnet operator {} · durable committee verified", options.id());
    let assets = assets();
    let max_expiries: usize = arg("--max-expiries", "2").parse()?;
    let tick: u64 = arg("--tick-secs", "5").parse()?;
    anyhow::ensure!(max_expiries > 0 && tick > 0, "cadences must be positive");
    // Relay runs separately so listing a ladder never starves settlement prices.
    let (relay_admin, relay_oracle, relay_seats) = (admin.clone(), oracle.clone(), seats.clone());
    let relay = tokio::spawn(async move {
        let mut printed = HashMap::new(); let mut complaints = HashMap::new(); let mut waited = vec![1000; relay_seats.len()];
        loop {
            match feed_committee(&relay_admin, &relay_oracle, &relay_seats, &mut printed, &mut complaints, &mut waited, ORACLE_SECS).await {
                Ok(n) => if n > 0 { println!("oracle accepted {n} signed observations"); },
                Err(error) => println!("oracle feed error: {error:#}"),
            }
            tokio::time::sleep(Duration::from_secs(ORACLE_SECS)).await;
        }
    });
    let mut last_list = Instant::now() - Duration::from_secs(60);
    let mut last_quotes = HashMap::new();
    loop {
        anyhow::ensure!(!relay.is_finished(), "oracle relay exited; refusing to continue unattended");
        let pass: Result<()> = async {
            let mut rows = chain_series(&options).await?;
            let now = worker.view_block().await?.timestamp();
            // Fix before doing slower listing work. The oracle has its own clock.
            let fixed = request_fixes(&admin, &options, &rows, now).await?;
            if fixed > 0 { println!("requested {fixed} expired settlements"); }
            for series_id in operator::voidable(&rows, now) {
                admin.call(options.id(), "void_series").args_json(json!({"series_id":series_id})).gas(GAS).transact().await?.into_result()?;
            }
            let expiries: Vec<u64> = engine_expiries(&engine).await?.into_iter().filter(|expiry| *expiry > now).take(max_expiries).collect();
            // Refresh existing quotes before creating more rows.
            for expiry in operator::ladders_due(&expiries, now, &last_quotes, max_expiries) {
                let live = HashMap::new();
                let (posted, _) = quote_ladder(&admin, &options, &engine, &assets, &rows, expiry, &live, now, 0).await?;
                last_quotes.insert(expiry, now);
                if posted > 0 { println!("quoted {posted} cells at {}", stamp(expiry)); }
            }
            if last_list.elapsed() >= Duration::from_secs(20) {
                // Small chunks keep quote refresh and expiry handling responsive.
                let (created, _) = list_ladder(&admin, &options, &engine, &assets, &rows, &expiries, 4).await?;
                last_list = Instant::now();
                if created > 0 { println!("listed {created} new series"); rows = chain_series(&options).await?; }
            }
            let _ = rows;
            Ok(())
        }.await;
        if let Err(error) = pass { println!("operator pass failed: {error:#}"); }
        tokio::time::sleep(Duration::from_secs(tick)).await;
    }
}
