// Match the crate's existing `oc_*` tests to OpenCode tests by name, writing
// <out>/ocmatch.tsv (rust file, fn, score, opencode file:line, name). Ports
// in tests/oc_*.rs are mapped explicitly in rules/zz_ports.txt instead.
// Usage: node match.js <crates/codesmith-core> <out>
const fs=require("fs")
const path=require("path")
const [crate,outDir]=process.argv.slice(2)
const tests=fs.readFileSync(path.join(outDir,"core_tests.jsonl"),"utf8").trim().split("\n").map(JSON.parse)
const fns=[]
const walk=(d)=>{for(const e of fs.readdirSync(d,{withFileTypes:true})){const p=path.join(d,e.name)
  if(e.isDirectory())walk(p)
  else if(e.name.endsWith(".rs")){const rel=path.relative(crate,p); if(/^tests\/oc_/.test(rel))continue
    for(const m of fs.readFileSync(p,"utf8").matchAll(/fn (oc_[a-z0-9_]+)/g))fns.push([rel,m[1]])}}}
walk(path.join(crate,"src"));walk(path.join(crate,"tests"))
const pre=[["command_",/^config\/command\./],["compaction_",/^(session-compaction|config\/compaction)\./],["config_",/^config\/(config|model|compaction)\./],["instructions_",/^instruction-discovery/],["image_",/^(tool-read|image-photon)\./],["permission_",/^permission\./],["session_retry_",/^session-runner-retry/],["model_retry_",/^session-runner-retry/],["session_error_",/^session-error/],["session_message_",/^session-runner-message/],["session_unanswered|session_invalid",/^session-runner/],["content_filter",/^session-runner-tool-events/],["skill_",/^config\/(markdown|skill)\./],["title_",/^session-title/],["edit_",/^tool-edit/],["webfetch_",/^tool-webfetch/],["html_markdown_budget",/^tool-html-markdown-budget/],["tool_output_",/^tool-output/],["read_",/^tool-read/],["scan_",/^shell-scan\//],["shell_parse_",/^shell-parse/],["session_environment",/^session-environment/],["shell_",/^tool-shell\./],["subagent_",/^(tool-subagent|subagent-report)/],["session_revert",/^session-revert/],["error_partial",/^session-runner/],["session_",/^session-runner/]]
const stop=new Set("a an the and or of to in on for with its it is be by as at from that this when".split(" "))
const tok=s=>s.toLowerCase().replace(/\$\{[^}]*\}/g," ").split(/[^a-z0-9]+/).filter(w=>w&&!stop.has(w))
const out=[]
for(const [file,fn] of fns){
  const name=fn.replace(/^oc_/,"")
  const p=pre.find(([k])=>new RegExp("^("+k+")").test(name))
  if(!p){out.push([file,fn,"NOPREFIX"]);continue}
  const rt=new Set(tok(name.replace(new RegExp("^("+p[0]+")"),"")))
  let best=null,bs=0
  for(const t of tests){ if(!p[1].test(t.file))continue
    const ot=new Set(tok(t.name)); let inter=0; for(const w of rt) if(ot.has(w)) inter++
    const s=inter/ (new Set([...rt,...ot]).size)
    if(s>bs){bs=s;best=t}}
  out.push([file,fn,bs.toFixed(2),best?best.file+":"+best.line:"",best?best.name:""])
}
fs.writeFileSync(path.join(outDir,"ocmatch.tsv"),out.map(r=>r.join("\t")).join("\n")+"\n")
