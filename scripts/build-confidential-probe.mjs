#!/usr/bin/env node
// Rust's WASI componentizer leaves Rust custom sections inside the core module.
// OutLayer reads the manifest from the outer component, so append it there.
import { readFileSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
const built=spawnSync("cargo",["build","-p","confidential-worker","--target","wasm32-wasip2","--release"],{stdio:"inherit"});
if(built.status!==0)process.exit(built.status??1);
const file="target/wasm32-wasip2/release/confidential-worker.wasm";
const input=readFileSync(file);
let offset=8; const chunks=[input.subarray(0,8)];
const readLeb=(at)=>{let value=0,shift=0;for(let i=0;i<5;i++){const b=input[at++];if(b===undefined)throw new Error("truncated component");value|=(b&127)<<shift;if(!(b&128))return[value>>>0,at];shift+=7;}throw new Error("invalid component length");};
while(offset<input.length){
 const start=offset,tag=input[offset++];const[size,body]=readLeb(offset);const end=body+size;if(end>input.length)throw new Error("invalid component section");
 let isManifest=false;if(tag===0){const[length,nameStart]=readLeb(body);isManifest=input.subarray(nameStart,nameStart+length).toString()==="outlayer.manifest";}
 if(!isManifest)chunks.push(input.subarray(start,end));offset=end;
}
const binary=Buffer.concat(chunks);
const leb=(n)=>{const bytes=[];do{let b=n&127;n>>>=7;if(n)b|=128;bytes.push(b)}while(n);return Buffer.from(bytes)};
const name=Buffer.from("outlayer.manifest");
const manifest=readFileSync("tools/confidential-worker/manifest.json");JSON.parse(manifest);
const section=Buffer.concat([leb(name.length),name,manifest]);
const output=Buffer.concat([binary,Buffer.from([0]),leb(section.length),section]);
writeFileSync(file,output);
console.log(JSON.stringify({file,bytes:output.length,sha256:createHash("sha256").update(output).digest("hex"),manifest:"outer component"}));
