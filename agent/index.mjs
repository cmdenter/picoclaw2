#!/usr/bin/env node
// ═══════════════════════════════════════════════════════════════════════
//  PicoClaw Dev Agent — 24/7 coding hands for the on-chain brain
//  Runs on Hetzner alongside SmartSUI. Polls PicoClaw for dev tasks,
//  executes git/file/shell ops, reports back.
// ═══════════════════════════════════════════════════════════════════════

import { HttpAgent, Actor } from "@dfinity/agent";
import { Ed25519KeyIdentity } from "@dfinity/identity";
import { readFileSync, writeFileSync, existsSync, mkdirSync, rmSync } from "fs";
import { execSync } from "child_process";
import { createServer } from "http";
import { join } from "path";

// ── Config ──────────────────────────────────────────────────────────
const CANISTER_ID = process.env.PICOCLAW_CANISTER_ID || "3rr66-6aaaa-aaaap-qqm6a-cai";
const ICP_HOST = "https://icp-api.io";
const WORKSPACE = process.env.WORKSPACE || join(process.cwd(), "workspace");
const PORT = parseInt(process.env.AGENT_PORT || "3847");
const POLL_INTERVAL = 30_000; // 30s
const KEY_FILE = join(process.cwd(), ".agent-identity.json");
const MAX_TURNS = 12; // max back-and-forth per task

// ── Identity (auto-generate + persist Ed25519 keypair) ──────────────
function loadOrCreateIdentity() {
  if (existsSync(KEY_FILE)) {
    const raw = JSON.parse(readFileSync(KEY_FILE, "utf-8"));
    return Ed25519KeyIdentity.fromJSON(JSON.stringify(raw));
  }
  const id = Ed25519KeyIdentity.generate();
  writeFileSync(KEY_FILE, JSON.stringify(id.toJSON()), { mode: 0o600 });
  return id;
}

const identity = loadOrCreateIdentity();
const principal = identity.getPrincipal().toText();
console.log(`[agent] principal: ${principal}`);
console.log(`[agent] ↑ Add this to PicoClaw allowed_callers to authorize`);

// ── Candid interface (inline — matches picoclaw.did) ────────────────
const idlFactory = ({ IDL }) => {
  return IDL.Service({
    chat: IDL.Func([IDL.Text], [IDL.Variant({ Ok: IDL.Text, Err: IDL.Text })], []),
    get_queue_length: IDL.Func([], [IDL.Nat64], ["query"]),
    cycle_balance: IDL.Func([], [IDL.Nat], ["query"]),
  });
};

// ── Actor ───────────────────────────────────────────────────────────
const agent = HttpAgent.createSync({ identity, host: ICP_HOST });
const actor = Actor.createActor(idlFactory, { agent, canisterId: CANISTER_ID });

// ── Shell helper — run with timeout, capture output ─────────────────
function sh(cmd, cwd, timeout = 60_000) {
  try {
    return execSync(cmd, {
      cwd: cwd || WORKSPACE,
      timeout,
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
      maxBuffer: 1024 * 1024,
    }).trim();
  } catch (e) {
    const stderr = e.stderr?.trim() || "";
    const stdout = e.stdout?.trim() || "";
    return `[exit ${e.status}] ${stderr || stdout}`.slice(0, 2000);
  }
}

// ── Workspace management ────────────────────────────────────────────
function ensureWorkspace() {
  if (!existsSync(WORKSPACE)) mkdirSync(WORKSPACE, { recursive: true });
}

function cloneOrPull(repo) {
  ensureWorkspace();
  const name = repo.split("/").pop().replace(".git", "");
  const dir = join(WORKSPACE, name);
  if (existsSync(join(dir, ".git"))) {
    sh("git checkout main 2>/dev/null || git checkout master", dir);
    sh("git pull --ff-only", dir);
  } else {
    sh(`git clone ${repo} ${name}`, WORKSPACE, 120_000);
  }
  return dir;
}

// ── Parse agent commands from LLM response ──────────────────────────
// PicoClaw outputs structured blocks the agent can execute:
//   [FILE:path] content [/FILE]
//   [RUN] command [/RUN]
//   [GIT:action] message [/GIT]
//   [DONE] summary [/DONE]

function parseCommands(text) {
  const cmds = [];

  // [FILE:path]...[/FILE]
  const fileRe = /\[FILE:([^\]]+)\]([\s\S]*?)\[\/FILE\]/g;
  let m;
  while ((m = fileRe.exec(text)) !== null) {
    cmds.push({ type: "file", path: m[1].trim(), content: m[2] });
  }

  // [RUN]...[/RUN]
  const runRe = /\[RUN\]([\s\S]*?)\[\/RUN\]/g;
  while ((m = runRe.exec(text)) !== null) {
    cmds.push({ type: "run", cmd: m[1].trim() });
  }

  // [GIT:action]...[/GIT]
  const gitRe = /\[GIT:(\w+)\]([\s\S]*?)\[\/GIT\]/g;
  while ((m = gitRe.exec(text)) !== null) {
    cmds.push({ type: "git", action: m[1].trim(), message: m[2].trim() });
  }

  // [DONE]
  if (text.includes("[DONE]")) {
    const doneRe = /\[DONE\]([\s\S]*?)(?:\[\/DONE\]|$)/;
    const dm = text.match(doneRe);
    cmds.push({ type: "done", summary: dm ? dm[1].trim() : "" });
  }

  return cmds;
}

// ── Execute parsed commands ─────────────────────────────────────────
function executeCommands(cmds, repoDir) {
  const results = [];

  for (const cmd of cmds) {
    switch (cmd.type) {
      case "file": {
        const full = join(repoDir, cmd.path);
        const dir = full.substring(0, full.lastIndexOf("/"));
        if (!existsSync(dir)) mkdirSync(dir, { recursive: true });
        writeFileSync(full, cmd.content);
        results.push(`wrote ${cmd.path} (${cmd.content.length} bytes)`);
        break;
      }
      case "run": {
        // Safety: block dangerous commands
        const lower = cmd.cmd.toLowerCase();
        if (lower.includes("rm -rf /") || lower.includes("sudo") || lower.includes("curl |")) {
          results.push(`blocked unsafe: ${cmd.cmd}`);
          break;
        }
        const out = sh(cmd.cmd, repoDir);
        const truncated = out.length > 1500 ? out.slice(0, 1500) + "..." : out;
        results.push(`$ ${cmd.cmd}\n${truncated}`);
        break;
      }
      case "git": {
        if (cmd.action === "branch") {
          results.push(sh(`git checkout -b ${cmd.message}`, repoDir));
        } else if (cmd.action === "commit") {
          sh("git add -A", repoDir);
          results.push(sh(`git commit -m "${cmd.message}"`, repoDir));
        } else if (cmd.action === "push") {
          results.push(sh("git push -u origin HEAD", repoDir));
        }
        break;
      }
      case "done":
        results.push(`[DONE] ${cmd.summary}`);
        break;
    }
  }

  return results;
}

// ── Dev task execution loop ─────────────────────────────────────────
async function runTask(task) {
  const { repo, prompt } = task;
  console.log(`[agent] task: ${prompt.slice(0, 80)}...`);

  // Clone/pull the repo
  let repoDir;
  try {
    repoDir = cloneOrPull(repo);
  } catch (e) {
    console.error(`[agent] clone failed: ${e.message}`);
    return;
  }

  // Get repo structure for context
  const tree = sh("find . -type f -not -path './.git/*' -not -path './node_modules/*' -not -path './target/*' | head -60", repoDir);

  // Initial prompt to PicoClaw
  const systemCtx = [
    `You are in DEV AGENT mode. You control a repo at ${repoDir}.`,
    `Output structured commands the agent executes:`,
    `[FILE:path]content[/FILE] — write a file`,
    `[RUN]command[/RUN] — run a shell command`,
    `[GIT:branch]name[/GIT] — create branch`,
    `[GIT:commit]message[/GIT] — stage + commit`,
    `[GIT:push][/GIT] — push to remote`,
    `[DONE]summary[/DONE] — task complete`,
    ``,
    `Repo files:\n${tree}`,
  ].join("\n");

  let conversation = `${systemCtx}\n\nTask: ${prompt}`;
  let done = false;

  for (let turn = 0; turn < MAX_TURNS && !done; turn++) {
    console.log(`[agent] turn ${turn + 1}/${MAX_TURNS}`);

    let reply;
    try {
      const r = await actor.chat(conversation);
      if (r?.Err) { console.error(`[agent] chat error: ${r.Err}`); break; }
      reply = r?.Ok || "";
    } catch (e) {
      console.error(`[agent] chat call failed: ${e.message}`);
      break;
    }

    console.log(`[agent] response: ${reply.slice(0, 120)}...`);

    const cmds = parseCommands(reply);
    if (cmds.length === 0) {
      // No structured commands — LLM is asking questions or giving text
      console.log(`[agent] no commands parsed, asking for structured output`);
      conversation = "Please respond with structured [FILE:], [RUN], [GIT:], or [DONE] commands. " + reply.slice(0, 500);
      continue;
    }

    const results = executeCommands(cmds, repoDir);
    console.log(`[agent] executed ${cmds.length} commands`);

    done = cmds.some((c) => c.type === "done");
    if (!done) {
      // Feed results back — PicoClaw decides next step
      conversation = "Execution results:\n" + results.join("\n---\n") + "\n\nContinue with next steps or [DONE] if complete.";
    } else {
      console.log(`[agent] task complete: ${results[results.length - 1]}`);
    }
  }

  if (!done) console.log(`[agent] max turns reached, stopping`);
}

// ── Task queue ──────────────────────────────────────────────────────
const taskQueue = [];
let working = false;

async function drainTasks() {
  if (working) return;
  working = true;
  while (taskQueue.length) {
    const task = taskQueue.shift();
    try {
      await runTask(task);
    } catch (e) {
      console.error(`[agent] task error: ${e.message}`);
    }
  }
  working = false;
}

// ── HTTP server — receive tasks via webhook ─────────────────────────
const server = createServer(async (req, res) => {
  // CORS
  res.setHeader("Access-Control-Allow-Origin", "*");
  if (req.method === "OPTIONS") { res.writeHead(200); res.end(); return; }

  if (req.method === "POST" && req.url === "/task") {
    let body = "";
    for await (const chunk of req) body += chunk;
    try {
      const { repo, prompt } = JSON.parse(body);
      if (!repo || !prompt) {
        res.writeHead(400);
        res.end(JSON.stringify({ error: "need repo + prompt" }));
        return;
      }
      taskQueue.push({ repo, prompt });
      drainTasks();
      res.writeHead(202);
      res.end(JSON.stringify({ queued: true, position: taskQueue.length }));
    } catch (e) {
      res.writeHead(400);
      res.end(JSON.stringify({ error: "invalid json" }));
    }
    return;
  }

  if (req.method === "GET" && req.url === "/status") {
    let cycles = "unknown";
    try {
      const bal = await actor.cycle_balance();
      cycles = (Number(bal) / 1e12).toFixed(2) + "T";
    } catch (_) {}
    res.writeHead(200);
    res.end(JSON.stringify({
      status: "ok",
      principal,
      queue: taskQueue.length,
      working,
      cycles,
    }));
    return;
  }

  res.writeHead(404);
  res.end(JSON.stringify({ error: "not found" }));
});

server.listen(PORT, () => {
  console.log(`[agent] listening on :${PORT}`);
  console.log(`[agent] POST /task  {"repo":"https://github.com/user/repo","prompt":"..."}`);
  console.log(`[agent] GET  /status`);
});
