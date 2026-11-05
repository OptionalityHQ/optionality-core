#!/usr/bin/env node
// Publish ONLY the nonfinancial capability probe. A failed function-call receipt
// is expected for FastFS data storage; its confirmed transaction is still indexed.
import * as nearAPI from "near-api-js";
import { readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
if(!process.argv.includes("--execute"))throw new Error("Pass --execute to publish the testnet probe");
const session=JSON.parse(readFileSync("data/testnet-session.json"));
if(session.network!=="testnet")throw new Error("testnet publication only");
const credential=process.env.NEAR_ADMIN_CREDENTIALS??join(homedir(),".near-credentials/testnet",session.admin.account+".json");
const c=JSON.parse(readFileSync(credential));
if(c.account_id && c.account_id!==session.admin.account)throw new Error("credential account mismatch");
const wasm=readFileSync("target/wasm32-wasip2/release/confidential-worker.wasm");
const hash=createHash("sha256").update(wasm).digest("hex");
const text=(s)=>{const b=Buffer.from(s),n=Buffer.alloc(4);n.writeUInt32LE(b.length);return Buffer.concat([n,b])};
const len=Buffer.alloc(4);len.writeUInt32LE(wasm.length);
const payload=Buffer.concat([Buffer.from([0]),text(hash+".wasm"),Buffer.from([1]),text("application/wasm"),len,wasm]);
const store=new nearAPI.keyStores.InMemoryKeyStore();await store.setKey("testnet",session.admin.account,nearAPI.utils.KeyPair.fromString(c.private_key??c.secret_key));
const near=await nearAPI.connect({networkId:"testnet",nodeUrl:session.rpc,keyStore:store});
const account=await near.account(session.admin.account);
let tx;
try{const result=await account.functionCall({contractId:"outlayer.testnet",methodName:"__fastdata_fastfs",args:payload,gas:1000000000000n,attachedDeposit:0n});tx=result.transaction.hash;}
catch(e){if(!e.transaction_outcome?.id || !/MethodNotFound/.test(String(e.message)))throw new Error("artifact publication was not confirmed; inspect transaction before retrying");tx=e.transaction_outcome.id;}
const url=`https://test.fastfs.io/${session.admin.account}/outlayer.testnet/${hash}.wasm`;
const artifact={network:"testnet",hash,url,publishTransaction:tx,purpose:"nonfinancial capability probe",publishedAt:new Date().toISOString()};
writeFileSync("data/confidential-probe-artifact.json",JSON.stringify(artifact,null,2)+"\n");
// Wait for indexing; do not deploy/run a URL before its bytes are verifiable.
let available=false;
for(let i=0;i<8;i++){
 const response=await fetch(url,{signal:AbortSignal.timeout(5000)});
 if(response.ok){const bytes=Buffer.from(await response.arrayBuffer());if(createHash("sha256").update(bytes).digest("hex")!==hash)throw new Error("artifact hash mismatch");available=true;break;}
 if(response.status!==404)throw new Error(`artifact HTTP ${response.status}`);
 await new Promise(r=>setTimeout(r,1000));
}
if(!available)throw new Error("confirmed publication is not indexed yet; check the recorded URL before retrying publication");
console.log(JSON.stringify({...artifact,artifactVerified:true},null,2));
