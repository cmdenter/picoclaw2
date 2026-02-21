import { picoclaw } from "declarations/picoclaw";
import { AuthClient } from "@dfinity/auth-client";
import { HttpAgent, Actor } from "@dfinity/agent";

// ── State ────────────────────────────────────────────────────────────
let actor = picoclaw; // Auto-connected via declarations
let authClient = null;
let identity = null;
let principalId = null;
let authProvider = null;
let sending = false;
let memPanelOpen = false;

// ── Auth ─────────────────────────────────────────────────────────────
async function initAuth() {
  try {
    authClient = await AuthClient.create();
    if (await authClient.isAuthenticated()) {
      identity = authClient.getIdentity();
      principalId = identity.getPrincipal().toText();
      authProvider = 'ii';
      await createAuthenticatedActor();
      updateAuthUI(true);
    }
  } catch(e) { console.warn('Auth init:', e.message); }
}

async function createAuthenticatedActor() {
  if (!identity) return;
  const agent = new HttpAgent({ identity, host: 'https://icp-api.io' });
  const { idlFactory } = await import("declarations/picoclaw");
  actor = Actor.createActor(idlFactory, {
    agent,
    canisterId: process.env.CANISTER_ID_PICOCLAW,
  });
}

function toggleAuthDropdown() {
  if (identity) { logout(); return; }
  document.getElementById('authDropdown').classList.toggle('show');
}

async function loginII() {
  document.getElementById('authDropdown').classList.remove('show');
  if (!authClient) authClient = await AuthClient.create();
  await new Promise((resolve, reject) => {
    authClient.login({
      identityProvider: 'https://identity.ic0.app',
      maxTimeToLive: BigInt(8) * BigInt(3_600_000_000_000),
      onSuccess: resolve,
      onError: reject,
    });
  });
  identity = authClient.getIdentity();
  principalId = identity.getPrincipal().toText();
  authProvider = 'ii';
  await createAuthenticatedActor();
  updateAuthUI(true);
  toast('II connected: ' + principalId.slice(0, 8) + '...');
  checkHealth();
  loadHistory();
}

async function loginPlug() {
  document.getElementById('authDropdown').classList.remove('show');
  if (!window.ic || !window.ic.plug) {
    toast('Plug wallet not found. Install the Plug extension.');
    window.open('https://plugwallet.ooo/', '_blank');
    return;
  }
  try {
    const canisterId = process.env.CANISTER_ID_PICOCLAW;
    await window.ic.plug.requestConnect({ whitelist: [canisterId], host: 'https://icp-api.io' });
    const p = await window.ic.plug.agent.getPrincipal();
    principalId = p.toText();
    identity = { getPrincipal: () => p, _plug: true };
    authProvider = 'plug';
    const { idlFactory } = await import("declarations/picoclaw");
    actor = await window.ic.plug.createActor({ canisterId, interfaceFactory: idlFactory });
    updateAuthUI(true);
    toast('Plug connected: ' + principalId.slice(0, 8) + '...');
    checkHealth();
    loadHistory();
  } catch(e) {
    console.error('Plug login failed:', e);
    toast('Plug connection failed');
  }
}

async function logout() {
  if (authProvider === 'plug' && window.ic?.plug) {
    try { await window.ic.plug.disconnect(); } catch(_) {}
  }
  if (authProvider === 'ii' && authClient) {
    await authClient.logout();
  }
  identity = null;
  principalId = null;
  authProvider = null;
  // Reset to anonymous actor
  const { picoclaw: anonActor } = await import("declarations/picoclaw");
  actor = anonActor;
  updateAuthUI(false);
  document.getElementById('statusDot').className = 'status-dot';
  toast('Disconnected');
}

function updateAuthUI(authenticated) {
  const bar = document.getElementById('authBar');
  if (authenticated && principalId) {
    const short = principalId.slice(0, 5) + '...' + principalId.slice(-3);
    const label = authProvider === 'plug' ? 'Plug' : 'II';
    bar.innerHTML = `<span class="principal-tag" title="${principalId}">${label}: ${short}</span><button class="auth-btn logout" onclick="window._pc.toggleAuthDropdown()">Disconnect</button>
      <div class="auth-dropdown" id="authDropdown"></div>`;
  } else {
    bar.innerHTML = `<button class="auth-btn" onclick="window._pc.toggleAuthDropdown()">Connect</button>
      <div class="auth-dropdown" id="authDropdown">
        <button onclick="window._pc.loginII()">Internet Identity</button>
        <button onclick="window._pc.loginPlug()">Plug Wallet</button>
      </div>`;
  }
}

// ── Health ────────────────────────────────────────────────────────────
async function checkHealth() {
  const dot = document.getElementById('statusDot');
  try {
    if (!actor) throw new Error('No actor');
    await actor.cycle_balance();
    dot.className = 'status-dot ok';
    refreshMetrics();
  } catch {
    dot.className = 'status-dot err';
  }
}

async function refreshMetrics() {
  if (!actor) return;
  try {
    const [m, bal, q] = await Promise.all([
      actor.get_metrics(),
      actor.cycle_balance(),
      actor.get_queue_length(),
    ]);
    document.getElementById('statMsgs').textContent = m.total_messages.toString();
    document.getElementById('statCalls').textContent = m.total_calls.toString();
    document.getElementById('statQueue').textContent = q.toString();
    const t = Number(bal) / 1e12;
    document.getElementById('statCycles').textContent = t.toFixed(2) + 'T';
  } catch {}
}

// ── History ──────────────────────────────────────────────────────────
async function loadHistory() {
  if (!actor) return;
  try {
    const msgs = await actor.get_history(BigInt(200));
    const area = document.getElementById('chatArea');
    area.innerHTML = '';
    if (msgs.length === 0) {
      area.innerHTML = '<div class="msg system">No messages yet. Start chatting!</div>';
    }
    msgs.forEach(m => addMessage(m.role, m.content, m.timestamp));
    scrollBottom();
  } catch (e) {
    toast('Failed: ' + e.message);
  }
}

// ── Chat ─────────────────────────────────────────────────────────────
async function sendMessage() {
  const input = document.getElementById('input');
  const text = input.value.trim();
  if (!text || sending) return;
  if (!identity) { toast('Connect a wallet first'); return; }
  if (!actor) { toast('Not connected'); return; }

  sending = true;
  input.value = '';
  autoResize(input);
  updateSendBtn();
  addMessage('user', text);
  scrollBottom();
  showTyping(true);

  try {
    const result = await actor.chat(text);
    showTyping(false);
    if ('Ok' in result) {
      addMessage('assistant', result.Ok);
    } else {
      addMessage('system', 'Error: ' + result.Err);
    }
    refreshMetrics();
    if (memPanelOpen) setTimeout(refreshMemory, 1500);
  } catch (e) {
    showTyping(false);
    addMessage('system', 'Call failed: ' + e.message);
  }
  sending = false;
  scrollBottom();
  updateSendBtn();
}

// ── UI helpers ───────────────────────────────────────────────────────
function addMessage(role, content, timestamp) {
  const area = document.getElementById('chatArea');
  const div = document.createElement('div');
  div.className = `msg ${role}`;
  div.textContent = content;
  if (timestamp) {
    const ts = document.createElement('span');
    ts.className = 'ts';
    const d = new Date(Number(timestamp) / 1e6);
    ts.textContent = d.toLocaleTimeString();
    div.appendChild(ts);
  }
  area.appendChild(div);
}

function scrollBottom() {
  const area = document.getElementById('chatArea');
  requestAnimationFrame(() => area.scrollTop = area.scrollHeight);
}

function showTyping(show) {
  const el = document.getElementById('typing');
  const area = document.getElementById('chatArea');
  if (show) { area.appendChild(el); el.classList.add('show'); }
  else { el.classList.remove('show'); }
  scrollBottom();
}

function autoResize(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, 120) + 'px';
  updateSendBtn();
}

function updateSendBtn() {
  document.getElementById('sendBtn').disabled = !document.getElementById('input').value.trim() || sending;
}

function handleKey(e) {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendMessage(); }
}

function toast(msg) {
  const el = document.getElementById('toast');
  el.textContent = msg;
  el.classList.add('show');
  setTimeout(() => el.classList.remove('show'), 2500);
}

// ── PicoMem ──────────────────────────────────────────────────────────
function toggleMemPanel() {
  memPanelOpen = !memPanelOpen;
  document.getElementById('memPanel').classList.toggle('show', memPanelOpen);
  document.getElementById('memToggleBtn').classList.toggle('active', memPanelOpen);
  if (memPanelOpen && actor) refreshMemory();
}

function renderTier(elId, sizeElId, text, maxChars) {
  const body = document.getElementById(elId);
  const size = document.getElementById(sizeElId);
  if (text && text.length > 0) {
    body.textContent = text;
    body.className = 'tier-body';
  } else {
    body.textContent = 'empty';
    body.className = 'tier-body empty';
  }
  size.textContent = (text ? text.length : 0) + '/' + maxChars;
}

async function refreshMemory() {
  if (!actor) return;
  try {
    const state = await actor.get_notes();
    renderTier('tierI', 'tierISize', state.identity, 256);
    renderTier('tierT', 'tierTSize', state.thread, 600);
    renderTier('tierE', 'tierESize', state.episodes, 900);
    renderTier('tierP', 'tierPSize', state.priors, 128);
    const ts = document.getElementById('memTs');
    if (state.updated_at > 0n) {
      const d = new Date(Number(state.updated_at) / 1e6);
      ts.textContent = 'updated ' + d.toLocaleTimeString();
    } else {
      ts.textContent = 'never compressed';
    }
    const filled = [state.identity, state.thread, state.episodes, state.priors].filter(s => s.length > 0).length;
    document.getElementById('memStatus').textContent = filled + '/4 tiers';
  } catch (e) {
    toast('Memory fetch failed: ' + e.message);
  }
}

async function triggerCompress() {
  if (!actor || !identity) { toast('Login first'); return; }
  const btn = document.getElementById('compressBtn');
  btn.disabled = true;
  btn.textContent = 'Compressing...';
  try {
    const result = await actor.compress_context();
    if ('Ok' in result) {
      toast('Compression complete');
      await refreshMemory();
    } else {
      toast('Compress error: ' + result.Err);
    }
  } catch (e) {
    toast('Compress failed: ' + e.message);
  }
  btn.disabled = false;
  btn.textContent = 'Compress Now';
}

async function clearMemory() {
  if (!actor || !identity) { toast('Login first'); return; }
  if (!confirm('Clear all PicoMem tiers? This cannot be undone.')) return;
  try {
    const result = await actor.clear_notes();
    if ('Ok' in result) {
      toast('Memory cleared');
      await refreshMemory();
    } else {
      toast('Clear error: ' + result.Err);
    }
  } catch (e) {
    toast('Clear failed: ' + e.message);
  }
}

// ── Wire up to window for onclick handlers ───────────────────────────
window._pc = {
  sendMessage, loadHistory, handleKey, autoResize,
  toggleAuthDropdown, loginII, loginPlug,
  toggleMemPanel, refreshMemory, triggerCompress, clearMemory,
};

document.getElementById('input').addEventListener('input', updateSendBtn);

// ── Init ─────────────────────────────────────────────────────────────
await initAuth();
checkHealth();
if (identity) {
  loadHistory();
  refreshMemory();
}
setInterval(refreshMetrics, 30000);
