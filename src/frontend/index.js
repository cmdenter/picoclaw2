import { picoclaw } from "declarations/picoclaw";
import { AuthClient } from "@dfinity/auth-client";
import { HttpAgent, Actor } from "@dfinity/agent";

// ── Cached DOM (single lookup at init, never re-queried) ────────────
const chatArea  = document.getElementById('chatArea');
const inputEl   = document.getElementById('input');
const sendBtn   = document.getElementById('sendBtn');
const typingEl  = document.getElementById('typing');
const statusDot = document.getElementById('statusDot');
const authBar   = document.getElementById('authBar');
const toastEl   = document.getElementById('toast');
const statMsgs  = document.getElementById('statMsgs');
const statCalls = document.getElementById('statCalls');
const statCycles= document.getElementById('statCycles');
const statQueue = document.getElementById('statQueue');
const statSpent = document.getElementById('statSpent');
const statCost  = document.getElementById('statCost');
const stopBtn   = document.getElementById('stopBtn');
const memPanel  = document.getElementById('memPanel');
const panelTabs = document.getElementById('panelTabs');
const memTabBtn = document.getElementById('memTabBtn');
const walletTabBtn = document.getElementById('walletTabBtn');
const tabInfo   = document.getElementById('tabInfo');
const memTs     = document.getElementById('memTs');
const webMemBody  = document.getElementById('webMemBody');
const webMemCount = document.getElementById('webMemCount');
const avatarImg   = document.getElementById('avatarImg');
const avatarSvg   = document.getElementById('avatarSvg');
const clawName    = document.getElementById('clawName');
const settingsModal = document.getElementById('settingsModal');
const profileNameInput = document.getElementById('profileName');
const nftGrid     = document.getElementById('nftGrid');
const customAvatarInput = document.getElementById('customAvatarUrl');
const nftBg       = document.getElementById('nftBg');
const keyHint     = document.getElementById('keyHint');
const keyChangeBtn = document.getElementById('keyChangeBtn');
const keyEdit     = document.getElementById('keyEdit');
const apiKeyInput = document.getElementById('apiKeyInput');
const modelDisplay = document.getElementById('modelDisplay');

// Wallet DOM refs
const walletPanel     = document.getElementById('walletPanel');
const walletAvailable = document.getElementById('walletAvailable');
const walletPending   = document.getElementById('walletPending');
const walletTotalIn   = document.getElementById('walletTotalIn');
const walletTotalOut  = document.getElementById('walletTotalOut');
const walletTxCount   = document.getElementById('walletTxCount');
const walletDepositAddr = document.getElementById('walletDepositAddr');
const walletTxList    = document.getElementById('walletTxList');
const walletStatus    = document.getElementById('walletStatus');
const withdrawAmountInput = document.getElementById('withdrawAmountInput');

// ── Defaults ─────────────────────────────────────────────────────────
const DEFAULT_AVATAR = 'https://5movr-diaaa-aaaak-aaftq-cai.raw.icp0.io/?type=thumbnail&tokenid=cgymy-lqkor-uwiaa-aaaaa-cqabm-4aqca-aabyj-q';

// ── State ────────────────────────────────────────────────────────────
let actor = picoclaw;
let authClient = null;
let identity = null;
let principalId = null;
let authProvider = null;
let activeTab = null; // null = closed, 'mem' = PicoMem, 'wallet' = Wallet
let toastTimer = 0;
let selectedNftUrl = '';
let devMode = false;
const devModeBtn = document.getElementById('devModeBtn');

// ── Message queue (never blocks the send button) ─────────────────────
const queue = [];
let processing = false;
let stopped = false;

// ── Tiny helpers (inlined on hot path) ──────────────────────────────
function syncSend() {
  sendBtn.disabled = !identity;
}

function scroll() {
  chatArea.scrollTop = chatArea.scrollHeight;
}

function appendMsg(role, text, ts) {
  const d = document.createElement('div');
  d.className = 'msg ' + role;
  d.textContent = text || '';
  if (ts) {
    const s = document.createElement('span');
    s.className = 'ts';
    s.textContent = new Date(Number(ts) / 1e6).toLocaleTimeString();
    d.appendChild(s);
  }
  chatArea.appendChild(d);
}

function toast(msg) {
  clearTimeout(toastTimer);
  toastEl.textContent = msg;
  toastEl.classList.add('show');
  toastTimer = setTimeout(() => toastEl.classList.remove('show'), 2500);
}

function autoResize(el) {
  const e = el || inputEl;
  e.style.height = 'auto';
  e.style.height = Math.min(e.scrollHeight, 120) + 'px';
}

function fmtCycles(n) {
  if (n >= 1e12) return (n / 1e12).toFixed(2) + 'T';
  if (n >= 1e9)  return (n / 1e9).toFixed(1) + 'B';
  if (n >= 1e6)  return (n / 1e6).toFixed(0) + 'M';
  return n.toString();
}

function handleKey(e) {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendMessage(); }
}

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
  // NFT gate — must own NFT to proceed
  try {
    const r = await actor.wallet_connect();
    if (r?.Err) {
      toast('NFT ownership required');
      await logout();
      return;
    }
  } catch (e) {
    toast('NFT verification failed');
    await logout();
    return;
  }
  updateAuthUI(true);
  toast('II connected: ' + principalId.slice(0, 8) + '...');
  syncSend();
  checkHealth();
  loadProfile();
  refreshWallet();
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
    // NFT gate — must own NFT to proceed
    try {
      const r = await actor.wallet_connect();
      if (r?.Err) {
        toast('NFT ownership required');
        await logout();
        return;
      }
    } catch (e2) {
      toast('NFT verification failed');
      await logout();
      return;
    }
    updateAuthUI(true);
    toast('Plug connected: ' + principalId.slice(0, 8) + '...');
    syncSend();
    checkHealth();
    loadProfile();
    refreshWallet();
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
  const { picoclaw: anonActor } = await import("declarations/picoclaw");
  actor = anonActor;
  updateAuthUI(false);
  syncSend();
  statusDot.className = 'status-dot';
  chatArea.innerHTML = '<div class="msg system">Session ended. Connect to start chatting.</div>';
  // Reset to default avatar and background
  clawName.textContent = 'PicoClaw';
  avatarImg.src = DEFAULT_AVATAR;
  avatarImg.style.display = '';
  avatarSvg.style.display = 'none';
  selectedNftUrl = DEFAULT_AVATAR;
  setNftBackground(DEFAULT_AVATAR);
  toast('Disconnected');
}

function updateAuthUI(authenticated) {
  if (authenticated && principalId) {
    const short = principalId.slice(0, 5) + '...' + principalId.slice(-3);
    const label = authProvider === 'plug' ? 'Plug' : 'II';
    authBar.innerHTML = `<span class="principal-tag" title="${principalId}">${label}: ${short}</span><button class="auth-btn logout" onclick="window._pc.toggleAuthDropdown()">Disconnect</button>
      <div class="auth-dropdown" id="authDropdown"></div>`;
    panelTabs.classList.add('authed');
  } else {
    authBar.innerHTML = `<button class="auth-btn" onclick="window._pc.toggleAuthDropdown()">Connect</button>
      <div class="auth-dropdown" id="authDropdown">
        <button onclick="window._pc.loginII()">Internet Identity</button>
        <button onclick="window._pc.loginPlug()">Plug Wallet</button>
      </div>`;
    panelTabs.classList.remove('authed');
    switchTab(null); // close any open panel
  }
}

// ── Health / Metrics ─────────────────────────────────────────────────
async function checkHealth() {
  try {
    if (!actor) throw 0;
    await actor.cycle_balance();
    statusDot.className = 'status-dot ok';
    refreshMetrics();
  } catch {
    statusDot.className = 'status-dot err';
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
    statMsgs.textContent = m.total_messages.toString();
    statCalls.textContent = m.total_calls.toString();
    statQueue.textContent = q.toString();
    statCycles.textContent = (Number(bal) / 1e12).toFixed(2) + 'T';
    const spent = Number(m.total_cycles_spent);
    statSpent.textContent = fmtCycles(spent);
    statSpent.title = m.total_calls > 0
      ? 'Avg: ' + fmtCycles(spent / Number(m.total_calls)) + '/call'
      : '';
    statCost.textContent = '$' + (spent * 1.33 / 1e12).toFixed(4);
  } catch {}
}


// ── History ──────────────────────────────────────────────────────────
async function loadHistory() {
  if (!actor) return;
  try {
    const msgs = await actor.get_history(BigInt(200));
    chatArea.innerHTML = '';
    if (!msgs.length) {
      chatArea.innerHTML = '<div class="msg system">No messages yet. Start chatting!</div>';
    }
    for (let i = 0; i < msgs.length; i++) {
      appendMsg(msgs[i].role, msgs[i].content, msgs[i].timestamp);
    }
    scroll();
  } catch (e) {
    toast('History failed: ' + (e?.message || e));
  }
}

// ── On-chain tools (free queries, skip LLM) ─────────────────────────
const PRINCIPAL_RE = /^[a-z0-9]{5}(-[a-z0-9]{5}){4,9}-[a-z0-9]{3}$/i;

function extractPrincipal(text) {
  // Exact match: just a principal
  const t = text.trim();
  if (PRINCIPAL_RE.test(t)) return t;
  // "convert <principal>" or "account id for <principal>" etc.
  const m = t.match(/([a-z0-9]{5}(?:-[a-z0-9]{5}){4,9}-[a-z0-9]{3})/i);
  return m ? m[1] : null;
}

async function handleToolQuery(text) {
  const principal = extractPrincipal(text);
  if (principal && actor.principal_to_account_id) {
    try {
      const r = await actor.principal_to_account_id(principal);
      if (r?.Ok) {
        return 'Account ID for ' + principal + ':\n' + r.Ok;
      }
      if (r?.Err) return 'Error: ' + r.Err;
    } catch (_) {}
  }
  return null; // no tool matched — fall through to LLM
}

// ── Dev mode toggle ──────────────────────────────────────────────────
function setDevMode() {
  devMode = !devMode;
  devModeBtn.classList.toggle('active', devMode);
  inputEl.placeholder = devMode ? '/dev task for coding agent...' : 'Message PicoClaw...';
}

// ── Chat (queue-based — button never blocks) ────────────────────────
function sendMessage() {
  const text = inputEl.value.trim();
  if (!text) return;
  if (!identity) return toast('Connect a wallet first');
  if (!actor) return toast('Not connected');

  inputEl.value = '';
  inputEl.style.height = 'auto';

  // In dev mode, prepend /dev so backend dispatches to agent
  const msg = devMode ? '/dev ' + text : text;
  appendMsg('user', msg);
  scroll();

  queue.push(msg);
  drainQueue();
}

async function drainQueue() {
  if (processing) return;
  processing = true;
  stopped = false;
  stopBtn.style.display = 'flex';
  while (queue.length && !stopped) {
    const text = queue.shift();
    chatArea.appendChild(typingEl);
    typingEl.classList.add('show');
    scroll();
    try {
      // Free on-chain tools — skip LLM if matched
      const toolResult = await handleToolQuery(text);
      if (toolResult) {
        appendMsg('assistant', toolResult);
      } else {
        const r = await actor.chat(text);
        if (stopped) break;
        if (r?.Ok != null) {
          appendMsg('assistant', r.Ok);
        } else if (r?.Err != null) {
          appendMsg('system', 'Error: ' + r.Err);
        } else {
          appendMsg('assistant', String(r ?? ''));
        }
        refreshMetrics();
        if (activeTab === 'mem') setTimeout(refreshMemory, 1500);
      }
    } catch (e) {
      if (!stopped) appendMsg('system', 'Call failed: ' + (e?.message || e));
    } finally {
      typingEl.classList.remove('show');
      scroll();
    }
  }
  processing = false;
  stopBtn.style.display = 'none';
}

function stopQueue() {
  stopped = true;
  queue.length = 0;
  typingEl.classList.remove('show');
  stopBtn.style.display = 'none';
  toast('Stopped — send a new message anytime');
}

// ── Tab switching ────────────────────────────────────────────────────
function switchTab(tab) {
  if (tab === activeTab) tab = null; // toggle off if same tab clicked
  activeTab = tab;
  memPanel.classList.toggle('show', tab === 'mem');
  walletPanel.classList.toggle('show', tab === 'wallet');
  memTabBtn.className = tab === 'mem' ? 'active-mem' : '';
  walletTabBtn.className = tab === 'wallet' ? 'active-wallet' : '';
  // Update info label
  if (tab === 'mem') {
    if (actor) refreshMemory();
  } else if (tab === 'wallet') {
    if (actor) refreshWallet();
  }
}

function renderTier(elId, sizeId, text, max) {
  const body = document.getElementById(elId);
  const size = document.getElementById(sizeId);
  const len = text ? text.length : 0;
  body.textContent = len ? text : 'empty';
  body.className = len ? 'tier-body' : 'tier-body empty';
  size.textContent = len + '/' + max;
}

function timeAgo(nsTimestamp) {
  const secs = Math.floor((Date.now() - Number(nsTimestamp) / 1e6) / 1000);
  if (secs < 60) return secs + 's ago';
  if (secs < 3600) return Math.floor(secs / 60) + 'm ago';
  return Math.floor(secs / 3600) + 'h ago';
}

async function refreshMemory() {
  if (!actor) return;
  try {
    const [s, webEntries] = await Promise.all([
      actor.get_notes(),
      actor.get_web_memory().catch(() => []),
    ]);
    renderTier('tierI', 'tierISize', s.identity, 256);
    renderTier('tierT', 'tierTSize', s.thread, 600);
    renderTier('tierE', 'tierESize', s.episodes, 900);
    renderTier('tierP', 'tierPSize', s.priors, 128);
    if (s.updated_at > 0n) {
      memTs.textContent = 'updated ' + new Date(Number(s.updated_at) / 1e6).toLocaleTimeString();
    } else {
      memTs.textContent = 'never compressed';
    }
    if (activeTab === 'mem') tabInfo.textContent = [s.identity, s.thread, s.episodes, s.priors].filter(x => x.length).length + '/4 tiers';

    // Web memory
    webMemCount.textContent = webEntries.length + '/12';
    if (webEntries.length === 0) {
      webMemBody.textContent = 'no lookups yet';
      webMemBody.className = 'tier-body empty';
    } else {
      const lines = webEntries.map(e => {
        const preview = e.summary.length > 80 ? e.summary.slice(0, 80) + '...' : e.summary;
        return e.url + ' (' + timeAgo(e.timestamp) + '): ' + preview;
      });
      webMemBody.textContent = lines.join('\n');
      webMemBody.className = 'tier-body';
    }
  } catch (e) {
    toast('Memory fetch failed: ' + (e?.message || e));
  }
}

async function triggerCompress() {
  if (!actor || !identity) return toast('Login first');
  const btn = document.getElementById('compressBtn');
  btn.disabled = true;
  btn.textContent = 'Compressing...';
  try {
    const r = await actor.compress_context();
    if (r?.Ok != null) { toast('Compression complete'); await refreshMemory(); }
    else toast('Compress error: ' + (r?.Err || 'unknown'));
  } catch (e) {
    toast('Compress failed: ' + (e?.message || e));
  }
  btn.disabled = false;
  btn.textContent = 'Compress Now';
}

async function clearMemory() {
  if (!actor || !identity) return toast('Login first');
  if (!confirm('Clear all PicoMem tiers? This cannot be undone.')) return;
  try {
    const [r, w] = await Promise.all([
      actor.clear_notes(),
      actor.clear_web_memory().catch(() => ({ Ok: null })),
    ]);
    if (r?.Ok != null) { toast('Memory cleared'); await refreshMemory(); }
    else toast('Clear error: ' + (r?.Err || 'unknown'));
  } catch (e) {
    toast('Clear failed: ' + (e?.message || e));
  }
}

// ── Profile / Settings ───────────────────────────────────────────────
function setNftBackground(url) {
  if (url) {
    nftBg.style.backgroundImage = 'url(' + url + ')';
    nftBg.classList.add('active');
    document.body.classList.add('has-nft-bg');
  } else {
    nftBg.style.backgroundImage = '';
    nftBg.classList.remove('active');
    document.body.classList.remove('has-nft-bg');
  }
}

async function loadProfile() {
  if (!actor || !identity) return;
  try {
    const p = await actor.get_profile();
    if (p.name && p.name !== 'PicoClaw') {
      clawName.textContent = p.name;
    } else {
      clawName.textContent = 'PicoClaw';
    }
    if (p.avatar_url) {
      avatarImg.src = p.avatar_url;
      avatarImg.style.display = '';
      avatarSvg.style.display = 'none';
      selectedNftUrl = p.avatar_url;
    } else {
      avatarImg.style.display = 'none';
      avatarSvg.style.display = '';
      selectedNftUrl = '';
    }
    setNftBackground(p.avatar_url);
  } catch (e) {
    console.warn('Profile load:', e.message || e);
  }
}

function openSettings() {
  if (!identity) return toast('Connect a wallet first');
  settingsModal.classList.add('show');
  profileNameInput.value = clawName.textContent === 'PicoClaw' ? '' : clawName.textContent;
  customAvatarInput.value = selectedNftUrl;
  // Reset key edit state
  keyEdit.classList.remove('show');
  keyChangeBtn.textContent = 'Change';
  apiKeyInput.value = '';
  // Load key hint + config info
  loadKeyHint();
  actor.get_config_public().then(cfg => {
    modelDisplay.textContent = 'Model: ' + cfg.model + '  |  Endpoint: ' + cfg.api_endpoint;
  }).catch(() => {});
  // Load NFTs if Plug is connected
  if (authProvider === 'plug' && window.ic?.plug) {
    loadUserNFTs();
  } else {
    nftGrid.innerHTML = '<div class="nft-empty">Connect Plug Wallet to browse NFTs</div>';
  }
}

function closeSettings() {
  settingsModal.classList.remove('show');
}

function loadUserNFTs() {
  // dab-js no longer on npm — show paste-URL guidance for Plug users
  nftGrid.innerHTML = '<div class="nft-empty">Paste your NFT image URL below (copy from Entrepot, Bioniq, or your collection page)</div>';
}

function selectNft(url) {
  selectedNftUrl = url;
  customAvatarInput.value = url;
  // Update selected state in grid
  for (const img of nftGrid.querySelectorAll('img')) {
    img.classList.toggle('selected', img.src === url);
  }
}

async function loadKeyHint() {
  if (!actor || !identity) return;
  try {
    const r = await actor.get_key_hint();
    keyHint.textContent = r?.Ok || 'not set';
  } catch {
    keyHint.textContent = 'unavailable';
  }
}

function toggleKeyEdit() {
  keyEdit.classList.toggle('show');
  if (keyEdit.classList.contains('show')) {
    apiKeyInput.value = '';
    apiKeyInput.focus();
    keyChangeBtn.textContent = 'Cancel';
  } else {
    keyChangeBtn.textContent = 'Change';
  }
}

async function saveProfile() {
  if (!actor || !identity) return toast('Not connected');
  const name = profileNameInput.value.trim() || 'PicoClaw';
  const url = customAvatarInput.value.trim();
  const newKey = apiKeyInput.value.trim();
  try {
    // Save API key if user entered one
    if (newKey) {
      const kr = await actor.set_api_key(newKey);
      if (kr?.Err != null) return toast('Key error: ' + kr.Err);
    }
    const r = await actor.set_profile(name, url);
    if (r?.Err != null) return toast('Error: ' + r.Err);
    // Update header
    clawName.textContent = name;
    if (url) {
      avatarImg.src = url;
      avatarImg.style.display = '';
      avatarSvg.style.display = 'none';
      selectedNftUrl = url;
    } else {
      avatarImg.style.display = 'none';
      avatarSvg.style.display = '';
      selectedNftUrl = '';
    }
    setNftBackground(url);
    closeSettings();
    toast(newKey ? 'Profile & key saved' : 'Profile saved');
  } catch (e) {
    toast('Save failed: ' + (e?.message || e));
  }
}

// Close settings on backdrop click
settingsModal.addEventListener('click', (e) => {
  if (e.target === settingsModal) closeSettings();
});

// ── Wallet ──────────────────────────────────────────────────────────
function fmtIcp(e8s) {
  const n = Number(e8s);
  return (n / 1e8).toFixed(4);
}

async function refreshHoldings() {
  if (!actor?.token_balances) return;
  const list = document.getElementById('holdingsList');
  try {
    const result = await actor.token_balances();
    const balances = result?.Ok || result;
    if (!balances || !balances.length) {
      list.innerHTML = '<div class="wallet-tx-empty">No tokens held</div>';
      return;
    }
    list.innerHTML = balances.map(b => {
      const dec = Number(b.decimals);
      const amount = (Number(b.balance_raw) / Math.pow(10, dec)).toFixed(dec > 6 ? 4 : dec > 2 ? 4 : 2);
      return '<div class="wallet-tx-item">' +
        '<span class="wallet-tx-type deposit">' + b.symbol + '</span>' +
        '<span class="wallet-tx-amount">' + amount + '</span>' +
        '</div>';
    }).join('');
  } catch (e) {
    list.innerHTML = '<div class="wallet-tx-empty">Failed to load holdings</div>';
  }
}

async function refreshWallet() {
  if (!actor || !identity) return;
  try {
    const [bal, addr, txs] = await Promise.all([
      actor.wallet_balance(),
      actor.wallet_deposit_address(),
      actor.wallet_tx_history(BigInt(50)).catch(() => []),
    ]);
    walletAvailable.textContent = fmtIcp(bal.available_e8s) + ' ICP';
    tabInfo.textContent = fmtIcp(bal.available_e8s) + ' ICP';
    walletPending.textContent = fmtIcp(bal.pending_e8s);
    walletTotalIn.textContent = fmtIcp(bal.total_deposited_e8s);
    walletTotalOut.textContent = fmtIcp(bal.total_withdrawn_e8s);
    walletTxCount.textContent = bal.tx_count.toString();
    walletDepositAddr.textContent = addr;
    renderTxHistory(txs);
    refreshHoldings(); // async, don't await — let it load independently
    walletStatus.textContent = bal.updated_at > 0n
      ? 'Updated ' + new Date(Number(bal.updated_at) / 1e6).toLocaleTimeString()
      : '';
  } catch (e) {
    walletStatus.textContent = 'Error: ' + (e?.message || e);
  }
}

function renderTxHistory(txs) {
  if (!txs || txs.length === 0) {
    walletTxList.innerHTML = '<div class="wallet-tx-empty">No transactions yet</div>';
    return;
  }
  walletTxList.innerHTML = '';
  for (const tx of txs) {
    const item = document.createElement('div');
    item.className = 'wallet-tx-item';
    const isDeposit = tx.tx_type === 0;
    const isFailed = tx.status === 2;
    const typeLabel = isDeposit ? 'IN' : 'OUT';
    const typeClass = isFailed ? 'failed' : (isDeposit ? 'deposit' : 'withdraw');
    const sign = isDeposit ? '+' : '-';
    item.innerHTML =
      '<span class="wallet-tx-type ' + typeClass + '">' + typeLabel + (isFailed ? ' FAIL' : '') + '</span>' +
      '<span class="wallet-tx-amount">' + sign + fmtIcp(tx.amount_e8s) + '</span>' +
      '<span class="wallet-tx-time">' + timeAgo(tx.timestamp) + '</span>';
    walletTxList.appendChild(item);
  }
}

function copyDepositAddr() {
  const addr = walletDepositAddr.textContent;
  if (!addr || addr === 'loading...') return;
  navigator.clipboard.writeText(addr).then(() => toast('Deposit address copied'));
}

async function notifyDeposit() {
  if (!actor || !identity) return toast('Connect a wallet first');
  const btn = document.getElementById('notifyDepositBtn');
  btn.disabled = true;
  btn.textContent = 'Checking...';
  walletStatus.textContent = 'Sweeping deposit...';
  try {
    const r = await actor.wallet_notify_deposit();
    if (r?.Ok != null) {
      toast(r.Ok);
      await refreshWallet();
    } else {
      toast(r?.Err || 'No deposit found');
      walletStatus.textContent = r?.Err || '';
    }
  } catch (e) {
    toast('Deposit check failed: ' + (e?.message || e));
    walletStatus.textContent = '';
  }
  btn.disabled = false;
  btn.textContent = 'Check External Deposit';
}

async function depositIcp() {
  if (!actor || !identity) return toast('Connect a wallet first');
  if (authProvider !== 'plug' || !window.ic?.plug) return toast('Deposit requires Plug wallet');
  const input = document.getElementById('depositAmountInput').value.trim();
  const amount = parseFloat(input);
  if (isNaN(amount) || amount <= 0) return toast('Enter a valid amount');
  const e8s = Math.round(amount * 1e8);
  const addr = walletDepositAddr.textContent;
  if (!addr || addr === 'loading...') return toast('Loading deposit address...');
  const btn = document.getElementById('depositBtn');
  btn.disabled = true;
  btn.textContent = 'Sending...';
  try {
    await window.ic.plug.requestTransfer({ to: addr, amount: e8s });
    toast('Transfer sent! Sweeping...');
    const r = await actor.wallet_notify_deposit();
    if (r?.Ok) toast(r.Ok);
    else toast(r?.Err || 'Sweep pending — try Check Deposit later');
    document.getElementById('depositAmountInput').value = '';
    await refreshWallet();
  } catch (e) {
    toast('Deposit failed: ' + (e?.message || e));
  }
  btn.disabled = false;
  btn.textContent = 'Deposit';
}

async function withdrawIcp() {
  if (!actor || !identity) return toast('Connect a wallet first');
  const input = withdrawAmountInput.value.trim();
  if (!input) return toast('Enter an amount');
  const amount = parseFloat(input);
  if (isNaN(amount) || amount <= 0) return toast('Invalid amount');
  const e8s = Math.round(amount * 1e8);
  if (e8s <= 0) return toast('Amount too small');

  const fee = 0.0001;
  if (!confirm('Withdraw ' + amount.toFixed(4) + ' ICP? (fee: ' + fee + ' ICP)')) return;

  const btn = document.getElementById('withdrawBtn');
  btn.disabled = true;
  btn.textContent = 'Processing...';
  walletStatus.textContent = 'Processing withdrawal...';
  try {
    const r = await actor.wallet_withdraw(BigInt(e8s));
    if (r?.Ok != null) {
      toast(r.Ok);
      withdrawAmountInput.value = '';
      await refreshWallet();
    } else {
      toast(r?.Err || 'Withdrawal failed');
      walletStatus.textContent = r?.Err || '';
    }
  } catch (e) {
    toast('Withdrawal failed: ' + (e?.message || e));
    walletStatus.textContent = '';
  }
  btn.disabled = false;
  btn.textContent = 'Withdraw';
}

// ── Wire up ─────────────────────────────────────────────────────────
window._pc = {
  sendMessage, loadHistory, handleKey, autoResize, stopQueue,
  toggleAuthDropdown, loginII, loginPlug,
  switchTab, refreshMemory, triggerCompress, clearMemory,
  openSettings, closeSettings, saveProfile, setDevMode, toggleKeyEdit,
  depositIcp, copyDepositAddr, notifyDeposit, withdrawIcp,
};

// ── Init ─────────────────────────────────────────────────────────────
// Show default avatar + background immediately
avatarImg.src = DEFAULT_AVATAR;
avatarImg.style.display = '';
avatarSvg.style.display = 'none';
setNftBackground(DEFAULT_AVATAR);

await initAuth();
syncSend();
checkHealth();
if (identity) {
  // Re-verify NFT for returning II sessions
  try {
    const r = await actor.wallet_connect();
    if (r?.Err) {
      toast('NFT ownership required');
      await logout();
    }
  } catch { await logout(); }
  if (identity) {
    loadProfile();
    refreshMemory();
    refreshWallet();
  }
} else {
  chatArea.innerHTML = '<div class="msg system">Connect a wallet to start chatting.</div>';
}
setInterval(refreshMetrics, 30000);
