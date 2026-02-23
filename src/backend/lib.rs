use candid::{CandidType, Deserialize, Principal};
use ic_cdk::management_canister::{
    http_request as mgmt_http_request, HttpHeader, HttpMethod, HttpRequestArgs, HttpRequestResult,
    VetKDCurve, VetKDDeriveKeyArgs, VetKDKeyId,
};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::storable::Bound;
use ic_stable_structures::{Cell, DefaultMemoryImpl, StableBTreeMap, Storable};
use hkdf::Hkdf;
use sha2::Sha256;
use std::borrow::Cow;
use std::cell::RefCell;

type Memory = VirtualMemory<DefaultMemoryImpl>;

// ═══════════════════════════════════════════════════════════════════════
//  Compact JSON helpers — replaces the entire serde_json dependency
// ═══════════════════════════════════════════════════════════════════════

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = std::fmt::Write::write_fmt(
                    &mut out,
                    format_args!("\\u{:04x}", c as u32),
                );
            }
            c => out.push(c),
        }
    }
    out
}

/// Extract the first `"content":"<value>"` from an OpenAI-compatible JSON response.
fn extract_content(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let needle = "\"content\":\"";
    let start = s.find(needle)? + needle.len();
    let rest = &s[start..];

    let mut result = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => return Some(result),
            '\\' => match chars.next()? {
                '"' => result.push('"'),
                '\\' => result.push('\\'),
                'n' => result.push('\n'),
                'r' => result.push('\r'),
                't' => result.push('\t'),
                '/' => result.push('/'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                        if let Some(c) = char::from_u32(cp) {
                            result.push(c);
                        }
                    }
                }
                c => {
                    result.push('\\');
                    result.push(c);
                }
            },
            c => result.push(c),
        }
    }
}

/// Extract `"prompt":"<value>"` from a simple JSON body.
fn extract_prompt(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let needle = "\"prompt\":\"";
    let start = s.find(needle)? + needle.len();
    let rest = &s[start..];

    let mut result = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => return Some(result),
            '\\' => match chars.next()? {
                '"' => result.push('"'),
                '\\' => result.push('\\'),
                'n' => result.push('\n'),
                c => {
                    result.push('\\');
                    result.push(c);
                }
            },
            c => result.push(c),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Binary serialization helpers — faster than Candid encode/decode
// ═══════════════════════════════════════════════════════════════════════

fn write_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn read_str(data: &[u8], pos: &mut usize) -> String {
    let len = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap()) as usize;
    *pos += 4;
    let s = String::from_utf8_lossy(&data[*pos..*pos + len]).into_owned();
    *pos += len;
    s
}

fn read_u32(data: &[u8], pos: &mut usize) -> u32 {
    let v = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    v
}

fn read_u64(data: &[u8], pos: &mut usize) -> u64 {
    let v = u64::from_le_bytes(data[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    v
}

// ═══════════════════════════════════════════════════════════════════════
//  Data types with efficient binary Storable implementations
// ═══════════════════════════════════════════════════════════════════════

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct AgentConfig {
    pub persona: String,
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub api_endpoint: String,
    pub max_context_messages: u32,
    pub max_response_bytes: u64,
    pub allowed_callers: Vec<Principal>,
    /// How many messages between automatic context compressions (0 = disabled).
    pub compress_interval: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            persona: "PicoClaw".into(),
            system_prompt: "You are PicoClaw, an on-chain AI on the Internet Computer. Be concise and helpful. Plain text only — no markdown, no **, no #. You MUST call the web_search tool for ANY question about current events, news, prices, weather, sports, stocks, or anything requiring up-to-date information. NEVER say you cannot browse the web. NEVER tell the user to check a website. ALWAYS use web_search instead. URLs in user messages are auto-scraped via [Web:]. Past lookups in [W].".into(),
            allowed_tools: vec![],
            api_key: None,
            model: "deepseek-ai/DeepSeek-V3".into(),
            api_endpoint: "https://llm.chutes.ai/v1/chat/completions".into(),
            max_context_messages: 1, // >0 = include truncated last-assistant reply for continuity
            max_response_bytes: 8192,
            allowed_callers: vec![],
            compress_interval: 4, // compress more often = smaller batches = cheaper + fresher notes
        }
    }
}

impl Storable for AgentConfig {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut buf = Vec::with_capacity(256);
        write_str(&mut buf, &self.persona);
        write_str(&mut buf, &self.system_prompt);
        buf.extend_from_slice(&(self.allowed_tools.len() as u32).to_le_bytes());
        for tool in &self.allowed_tools {
            write_str(&mut buf, tool);
        }
        match &self.api_key {
            Some(k) => { buf.push(1); write_str(&mut buf, k); }
            None => buf.push(0),
        }
        write_str(&mut buf, &self.model);
        write_str(&mut buf, &self.api_endpoint);
        buf.extend_from_slice(&self.max_context_messages.to_le_bytes());
        buf.extend_from_slice(&self.max_response_bytes.to_le_bytes());
        // allowed_callers
        buf.extend_from_slice(&(self.allowed_callers.len() as u32).to_le_bytes());
        for principal in &self.allowed_callers {
            let pb = principal.as_slice();
            buf.push(pb.len() as u8);
            buf.extend_from_slice(pb);
        }
        // compress_interval
        buf.extend_from_slice(&self.compress_interval.to_le_bytes());
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let d = bytes.as_ref();
        let mut p = 0;
        let persona = read_str(d, &mut p);
        let system_prompt = read_str(d, &mut p);
        let n_tools = read_u32(d, &mut p) as usize;
        let mut allowed_tools = Vec::with_capacity(n_tools);
        for _ in 0..n_tools {
            allowed_tools.push(read_str(d, &mut p));
        }
        let api_key = if d[p] == 1 { p += 1; Some(read_str(d, &mut p)) } else { p += 1; None };
        let model = read_str(d, &mut p);
        let api_endpoint = read_str(d, &mut p);
        let max_context_messages = read_u32(d, &mut p);
        let max_response_bytes = read_u64(d, &mut p);
        // allowed_callers (may be absent in old data)
        let mut allowed_callers = Vec::new();
        if p < d.len() {
            let n_callers = read_u32(d, &mut p) as usize;
            allowed_callers.reserve(n_callers);
            for _ in 0..n_callers {
                let plen = d[p] as usize;
                p += 1;
                allowed_callers.push(Principal::from_slice(&d[p..p + plen]));
                p += plen;
            }
        }
        // compress_interval (may be absent in old data)
        let compress_interval = if p + 4 <= d.len() { read_u32(d, &mut p) } else { 6 };
        Self { persona, system_prompt, allowed_tools, api_key, model, api_endpoint, max_context_messages, max_response_bytes, allowed_callers, compress_interval }
    }

    const BOUND: Bound = Bound::Bounded { max_size: 8192, is_fixed_size: false };
}

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub timestamp: u64,
}

impl Storable for Message {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut buf = Vec::with_capacity(self.content.len() + 32);
        write_str(&mut buf, &self.role);
        write_str(&mut buf, &self.content);
        buf.extend_from_slice(&self.timestamp.to_le_bytes());
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let d = bytes.as_ref();
        let mut p = 0;
        let role = read_str(d, &mut p);
        let content = read_str(d, &mut p);
        let timestamp = read_u64(d, &mut p);
        Self { role, content, timestamp }
    }

    const BOUND: Bound = Bound::Bounded { max_size: 16384, is_fixed_size: false };
}

#[derive(CandidType, Deserialize, Clone, Debug, Default)]
pub struct Metrics {
    pub total_calls: u64,
    pub total_cycles_spent: u64,
    pub total_messages: u64,
    pub errors: u64,
}

impl Storable for Metrics {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut buf = Vec::with_capacity(32);
        buf.extend_from_slice(&self.total_calls.to_le_bytes());
        buf.extend_from_slice(&self.total_cycles_spent.to_le_bytes());
        buf.extend_from_slice(&self.total_messages.to_le_bytes());
        buf.extend_from_slice(&self.errors.to_le_bytes());
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let d = bytes.as_ref();
        Self {
            total_calls: u64::from_le_bytes(d[0..8].try_into().unwrap()),
            total_cycles_spent: u64::from_le_bytes(d[8..16].try_into().unwrap()),
            total_messages: u64::from_le_bytes(d[16..24].try_into().unwrap()),
            errors: u64::from_le_bytes(d[24..32].try_into().unwrap()),
        }
    }

    const BOUND: Bound = Bound::Bounded { max_size: 32, is_fixed_size: true };
}

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct UserProfile {
    pub name: String,       // max 32 chars — custom PicoClaw name
    pub avatar_url: String, // max 256 chars — NFT image URL
    pub updated_at: u64,
}

impl Default for UserProfile {
    fn default() -> Self {
        Self { name: "PicoClaw".into(), avatar_url: String::new(), updated_at: 0 }
    }
}

impl Storable for UserProfile {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut buf = Vec::with_capacity(self.name.len() + self.avatar_url.len() + 16);
        write_str(&mut buf, &self.name);
        write_str(&mut buf, &self.avatar_url);
        buf.extend_from_slice(&self.updated_at.to_le_bytes());
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let d = bytes.as_ref();
        let mut p = 0;
        let name = read_str(d, &mut p);
        let avatar_url = read_str(d, &mut p);
        let updated_at = read_u64(d, &mut p);
        Self { name, avatar_url, updated_at }
    }

    const BOUND: Bound = Bound::Bounded { max_size: 512, is_fixed_size: false };
}

/// Tiered conversation state — the PicoClaw equivalent of memory.
/// Fixed-size, RWKV-inspired: each tier has its own decay policy.
///   I: Identity — permanent KV facts (never decay)
///   T: Thread   — current conversation focus (replaced each compression)
///   E: Episodes — rolling topic history (FIFO decay)
///   P: Priors   — behavioral signals (Wasm-tracked, EMA decay, zero-cost)
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct PicoState {
    pub identity: String,   // max 256 chars — permanent KV facts (name,project,tech,prefs)
    pub thread: String,     // max 600 chars — current thread summary, telegram-style
    pub episodes: String,   // max 900 chars — rolling episode history, semicolon-delimited
    pub priors: String,     // max 128 chars — behavioral counters (Wasm-managed, FREE)
    pub updated_at: u64,
    pub msg_id_at_compress: u64,
}

impl Default for PicoState {
    fn default() -> Self {
        Self {
            identity: String::new(), thread: String::new(),
            episodes: String::new(), priors: String::new(),
            updated_at: 0, msg_id_at_compress: 0,
        }
    }
}

impl Storable for PicoState {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut buf = Vec::with_capacity(
            self.identity.len() + self.thread.len() + self.episodes.len()
            + self.priors.len() + 40
        );
        write_str(&mut buf, &self.identity);
        write_str(&mut buf, &self.thread);
        write_str(&mut buf, &self.episodes);
        write_str(&mut buf, &self.priors);
        buf.extend_from_slice(&self.updated_at.to_le_bytes());
        buf.extend_from_slice(&self.msg_id_at_compress.to_le_bytes());
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let d = bytes.as_ref();
        let mut p = 0;
        let first_str = read_str(d, &mut p);
        // Migration: old SessionNotes = 1 string + 2 u64s (exactly 16 bytes remain)
        let remaining = d.len() - p;
        if remaining == 16 {
            let updated_at = read_u64(d, &mut p);
            let msg_id_at_compress = read_u64(d, &mut p);
            return Self {
                identity: String::new(),
                thread: first_str, // old notes → thread tier
                episodes: String::new(),
                priors: String::new(),
                updated_at, msg_id_at_compress,
            };
        }
        // New PicoState format: 4 strings + 2 u64s
        let thread = read_str(d, &mut p);
        let episodes = read_str(d, &mut p);
        let priors = read_str(d, &mut p);
        let updated_at = read_u64(d, &mut p);
        let msg_id_at_compress = read_u64(d, &mut p);
        Self { identity: first_str, thread, episodes, priors, updated_at, msg_id_at_compress }
    }

    const BOUND: Bound = Bound::Bounded { max_size: 8192, is_fixed_size: false };
}

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct WebEntry {
    pub url: String,
    pub summary: String,
    pub timestamp: u64,
}

impl Storable for WebEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut buf = Vec::with_capacity(self.url.len() + self.summary.len() + 24);
        write_str(&mut buf, &self.url);
        write_str(&mut buf, &self.summary);
        buf.extend_from_slice(&self.timestamp.to_le_bytes());
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let d = bytes.as_ref();
        let mut p = 0;
        let url = read_str(d, &mut p);
        let summary = read_str(d, &mut p);
        let timestamp = read_u64(d, &mut p);
        Self { url, summary, timestamp }
    }

    const BOUND: Bound = Bound::Bounded { max_size: 2048, is_fixed_size: false };
}

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct QueuedTask {
    pub prompt: String,
    pub caller: Principal,
    pub created_at: u64,
}

impl Storable for QueuedTask {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut buf = Vec::with_capacity(self.prompt.len() + 48);
        write_str(&mut buf, &self.prompt);
        let pb = self.caller.as_slice();
        buf.push(pb.len() as u8);
        buf.extend_from_slice(pb);
        buf.extend_from_slice(&self.created_at.to_le_bytes());
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let d = bytes.as_ref();
        let mut p = 0;
        let prompt = read_str(d, &mut p);
        let plen = d[p] as usize;
        p += 1;
        let caller = Principal::from_slice(&d[p..p + plen]);
        p += plen;
        let created_at = read_u64(d, &mut p);
        Self { prompt, caller, created_at }
    }

    const BOUND: Bound = Bound::Bounded { max_size: 8192, is_fixed_size: false };
}

/// Opaque wrapper for storing a secret in its own stable Cell.
/// Stores either VetKey-encrypted bytes (new format) or legacy plaintext.
/// Never exposed via any query or Candid interface.
#[derive(Clone)]
struct SecretString(Vec<u8>);

impl Default for SecretString {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl Storable for SecretString {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.to_vec())
    }

    const BOUND: Bound = Bound::Bounded { max_size: 512, is_fixed_size: false };
}

// ═══════════════════════════════════════════════════════════════════════
//  VetKey encryption — API key encrypted at rest via ICP threshold BLS
// ═══════════════════════════════════════════════════════════════════════

/// Magic header for VetKey-encrypted data: "VK" + version 1
const ENC_MAGIC: [u8; 3] = [0x56, 0x4B, 0x01];
const ENC_NONCE_LEN: usize = 16;

/// BLS12-381 G1 identity element (point at infinity, compressed).
/// Using this as transport_public_key gives back an unencrypted VetKey.
const G1_IDENTITY: [u8; 48] = {
    let mut b = [0u8; 48];
    b[0] = 0xC0; // compressed + infinity flags
    b
};
const G1_BYTES: usize = 48;
const G2_BYTES: usize = 96;
/// Offset of the unencrypted VetKey (c3 component) in the derive_key response.
const VETKEY_C3_OFFSET: usize = G1_BYTES + G2_BYTES; // 144

fn vetkd_key_id() -> VetKDKeyId {
    VetKDKeyId {
        curve: VetKDCurve::Bls12_381_G2,
        name: "test_key_1".to_string(),
    }
}

/// Derive a VetKey by calling the management canister with identity transport key.
/// The result is a 48-byte BLS signature that only this canister can derive.
async fn derive_vetkey_bytes() -> Result<[u8; G1_BYTES], String> {
    let args = VetKDDeriveKeyArgs {
        input: b"picoclaw-api-key".to_vec(),
        context: b"picoclaw-encryption".to_vec(),
        key_id: vetkd_key_id(),
        transport_public_key: G1_IDENTITY.to_vec(),
    };

    let result = ic_cdk::management_canister::vetkd_derive_key(&args)
        .await
        .map_err(|e| format!("VetKD derive key failed: {:?}", e))?;

    let enc = &result.encrypted_key;
    if enc.len() < VETKEY_C3_OFFSET + G1_BYTES {
        return Err("Invalid VetKD response length".into());
    }

    let mut vk = [0u8; G1_BYTES];
    vk.copy_from_slice(&enc[VETKEY_C3_OFFSET..VETKEY_C3_OFFSET + G1_BYTES]);
    Ok(vk)
}

/// Get cached VetKey bytes or derive fresh ones from the management canister.
async fn get_or_derive_vetkey() -> Result<[u8; G1_BYTES], String> {
    let cached = VETKEY_CACHE.with(|c| *c.borrow());
    if let Some(vk) = cached {
        return Ok(vk);
    }
    let vk = derive_vetkey_bytes().await?;
    VETKEY_CACHE.with(|c| *c.borrow_mut() = Some(vk));
    Ok(vk)
}

/// Derive a keystream of `len` bytes from VetKey material + nonce using HKDF-SHA256.
fn derive_keystream(vetkey: &[u8; G1_BYTES], nonce: &[u8], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(nonce), vetkey);
    let mut okm = vec![0u8; len];
    hk.expand(b"picoclaw-api-key-v1", &mut okm)
        .expect("HKDF output length <= 255*32");
    okm
}

/// XOR-encrypt (or decrypt) `data` using HKDF-derived keystream.
fn xor_with_keystream(vetkey: &[u8; G1_BYTES], nonce: &[u8], data: &[u8]) -> Vec<u8> {
    let ks = derive_keystream(vetkey, nonce, data.len());
    data.iter().zip(ks.iter()).map(|(d, k)| d ^ k).collect()
}

/// Check if stored bytes use the VetKey-encrypted format.
fn is_vetkey_encrypted(data: &[u8]) -> bool {
    data.len() >= ENC_MAGIC.len() + ENC_NONCE_LEN + 1
        && data[..ENC_MAGIC.len()] == ENC_MAGIC
}

// ═══════════════════════════════════════════════════════════════════════
//  Stable state
// ═══════════════════════════════════════════════════════════════════════

thread_local! {
    /// Cached VetKey bytes (48) — derived on demand, cleared on upgrade.
    static VETKEY_CACHE: RefCell<Option<[u8; G1_BYTES]>> = RefCell::new(None);

    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));

    // Cell: O(1) direct read/write — no B-tree overhead for singletons
    static CONFIG: RefCell<Cell<AgentConfig, Memory>> = RefCell::new(
        Cell::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(0))), AgentConfig::default())
            .expect("config cell init")
    );
    static METRICS_STORE: RefCell<Cell<Metrics, Memory>> = RefCell::new(
        Cell::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(2))), Metrics::default())
            .expect("metrics cell init")
    );
    static SESSION_NOTES: RefCell<Cell<PicoState, Memory>> = RefCell::new(
        Cell::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(4))), PicoState::default())
            .expect("picostate cell init")
    );

    // BTreeMap: for keyed collections that need range queries or deletion
    static CHAT_LOG: RefCell<StableBTreeMap<u64, Message, Memory>> = RefCell::new(
        StableBTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(1))))
    );
    static TASK_QUEUE: RefCell<StableBTreeMap<u64, QueuedTask, Memory>> = RefCell::new(
        StableBTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(3))))
    );

    // Web memory: ring buffer of 12 entries (MemoryId 5) + counter (MemoryId 6)
    static WEB_MEM: RefCell<StableBTreeMap<u8, WebEntry, Memory>> = RefCell::new(
        StableBTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(5))))
    );
    static WEB_COUNTER: RefCell<Cell<u64, Memory>> = RefCell::new(
        Cell::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(6))), 0u64)
            .expect("web counter init")
    );

    static USER_PROFILE: RefCell<Cell<UserProfile, Memory>> = RefCell::new(
        Cell::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(7))), UserProfile::default())
            .expect("user profile cell init")
    );

    // API key stored separately — never exposed via any query endpoint
    static API_KEY_STORE: RefCell<Cell<SecretString, Memory>> = RefCell::new(
        Cell::init(MEMORY_MANAGER.with(|m| m.borrow().get(MemoryId::new(8))), SecretString::default())
            .expect("api key cell init")
    );

    static MSG_COUNTER: RefCell<u64> = RefCell::new(0);
    static TASK_COUNTER: RefCell<u64> = RefCell::new(0);
}

// ═══════════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════════

fn get_config() -> AgentConfig {
    CONFIG.with(|c| c.borrow().get().clone())
}

/// Read the API key from its dedicated secure cell (never exposed via queries).
/// Handles both VetKey-encrypted (new) and plaintext (legacy) formats.
async fn get_api_key() -> Option<String> {
    let data = API_KEY_STORE.with(|k| k.borrow().get().0.clone());
    if data.is_empty() {
        return None;
    }

    if is_vetkey_encrypted(&data) {
        // Encrypted format: magic(3) || nonce(16) || ciphertext
        let nonce = &data[ENC_MAGIC.len()..ENC_MAGIC.len() + ENC_NONCE_LEN];
        let ciphertext = &data[ENC_MAGIC.len() + ENC_NONCE_LEN..];
        let vk = get_or_derive_vetkey().await.ok()?;
        let plaintext = xor_with_keystream(&vk, nonce, ciphertext);
        String::from_utf8(plaintext).ok()
    } else {
        // Legacy plaintext format
        String::from_utf8(data).ok().filter(|s| !s.is_empty())
    }
}

fn require_controller() -> Result<(), String> {
    let caller = ic_cdk::api::msg_caller();
    if caller == Principal::anonymous() || !ic_cdk::api::is_controller(&caller) {
        return Err("Access denied: controller only".into());
    }
    Ok(())
}

/// Check if the caller is authorized (controller OR on the allowlist).
/// Rejects the anonymous principal — frontend must authenticate via Internet Identity.
fn require_authorized() -> Result<(), String> {
    let caller = ic_cdk::api::msg_caller();
    if caller == Principal::anonymous() {
        return Err("Anonymous calls not allowed — authenticate with Internet Identity".into());
    }
    if ic_cdk::api::is_controller(&caller) {
        return Ok(());
    }
    let callers = CONFIG.with(|c| c.borrow().get().allowed_callers.clone());
    // If allowlist is empty, permit any authenticated principal
    if callers.is_empty() || callers.iter().any(|p| *p == caller) {
        Ok(())
    } else {
        Err("Access denied".into())
    }
}

fn bump_metric(f: impl FnOnce(&mut Metrics)) {
    METRICS_STORE.with(|m| {
        let mut cell = m.borrow_mut();
        let mut metrics = cell.get().clone();
        f(&mut metrics);
        let _ = cell.set(metrics);
    });
}

fn next_msg_id() -> u64 {
    MSG_COUNTER.with(|c| {
        let mut id = c.borrow_mut();
        *id += 1;
        *id
    })
}

fn log_message(role: &str, content: &str) {
    let id = next_msg_id();
    CHAT_LOG.with(|c| {
        c.borrow_mut().insert(id, Message {
            role: role.into(),
            content: content.into(),
            timestamp: ic_cdk::api::time(),
        });
    });
    bump_metric(|m| m.total_messages += 1);
    // Free Wasm-side priors update on every user message
    if role == "user" {
        update_priors(content);
    }
}


const MAX_PROMPT_BYTES: usize = 4096;

// PicoState tier budget constants (total: ~2000 chars ~= 650 tokens ~= 2 KB)
const LAST_REPLY_MAX_CHARS: usize = 300;  // Truncate last assistant reply for continuity
const MAX_IDENTITY_CHARS: usize = 256;    // I: permanent KV facts (never decay)
const MAX_THREAD_CHARS: usize = 600;      // T: current thread summary (replaced each compression)
const MAX_EPISODES_CHARS: usize = 900;    // E: rolling episode history (FIFO decay)
const MAX_PRIORS_CHARS: usize = 128;      // P: behavioral counters (Wasm-tracked, free)
const TRANSCRIPT_MSG_MAX_CHARS: usize = 200; // Truncate each msg before sending to compressor

/// Truncate a string at a UTF-8 char boundary.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── PicoState: Wasm-side extractors (zero cycle cost) ──────────────────

/// Parse priors string "n=12|al=180|qr=30|cr=5" → (n, avg_len, question_rate, code_rate)
fn parse_priors(s: &str) -> (u32, u32, u32, u32) {
    let (mut n, mut al, mut qr, mut cr) = (0u32, 0u32, 0u32, 0u32);
    for pair in s.split('|') {
        if let Some((key, val)) = pair.split_once('=') {
            match key.trim() {
                "n"  => n  = val.parse().unwrap_or(0),
                "al" => al = val.parse().unwrap_or(0),
                "qr" => qr = val.parse().unwrap_or(0),
                "cr" => cr = val.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    (n, al, qr, cr)
}

/// Update behavioral priors from user message — runs in Wasm, zero cycles.
/// Tracks: n=turn count, al=avg msg length, qr=question %, cr=code %.
/// Uses integer EMA (85/15 decay ≈ alpha=0.15).
fn update_priors(user_msg: &str) {
    SESSION_NOTES.with(|s| {
        let mut cell = s.borrow_mut();
        let mut state = cell.get().clone();
        let (mut n, mut al, mut qr, mut cr) = parse_priors(&state.priors);

        let len = user_msg.len() as u32;
        let has_q = if user_msg.contains('?') { 100u32 } else { 0 };
        let has_code = if user_msg.contains("```") || user_msg.contains("fn ")
            || user_msg.contains("let ") || user_msg.contains("pub ") { 100 } else { 0u32 };

        if n == 0 {
            // Seed with first observation
            al = len; qr = has_q; cr = has_code;
        } else {
            // Integer EMA: new = old*85/100 + sample*15/100
            al = (al * 85 + len * 15) / 100;
            qr = (qr * 85 + has_q * 15) / 100;
            cr = (cr * 85 + has_code * 15) / 100;
        }
        n += 1;

        // Format and cap
        let priors = format!("n={}|al={}|qr={}|cr={}", n, al, qr, cr);
        state.priors = truncate_utf8(&priors, MAX_PRIORS_CHARS).to_string();
        let _ = cell.set(state);
    });
}

/// Parse multi-tier compression output (I:/T:/E: lines) from LLM.
fn parse_tiers(output: &str) -> (String, String, String) {
    let mut identity = String::new();
    let mut thread = String::new();
    let mut episodes = String::new();
    let mut target: u8 = 0; // 0=none, 1=I, 2=T, 3=E

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("I:") {
            identity = rest.trim().to_string();
            target = 1;
        } else if let Some(rest) = trimmed.strip_prefix("T:") {
            thread = rest.trim().to_string();
            target = 2;
        } else if let Some(rest) = trimmed.strip_prefix("E:") {
            episodes = rest.trim().to_string();
            target = 3;
        } else if !trimmed.is_empty() {
            // Continuation line — append to current target
            let t = match target {
                1 => &mut identity,
                2 => &mut thread,
                3 => &mut episodes,
                _ => continue,
            };
            if !t.is_empty() { t.push(' '); }
            t.push_str(trimmed);
        }
    }

    (identity, thread, episodes)
}


// ── Web browsing helpers ───────────────────────────────────────────────

fn extract_url(text: &str) -> Option<&str> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let rest = &text[start..];
    let end = rest.find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '>' || c == ')').unwrap_or(rest.len());
    Some(&rest[..end])
}

// ── SmartSUI server constants ─────────────────────────────────────────
const PICO_SERVER_URL: &str = "https://smartsui.io/api/intel";
const PICO_SERVER_KEY: &str = "pico_ca06ade4ccc876a78cb50b7091cd0189ad77984af38f9d8e627214f97a9ef10d";

// ── Dev Agent (Hetzner) ──────────────────────────────────────────────
const DEV_AGENT_URL: &str = "https://smartsui.io:3847/task";
const DEV_DEFAULT_REPO: &str = "https://github.com/cmdenter/picoclaw2";

/// Extract the "f" (facts) field from a server /api/intel JSON response.
fn extract_intel_facts(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    // Check "ok":true
    if !s.contains("\"ok\":true") && !s.contains("\"ok\": true") {
        return None;
    }
    // Extract "f":"<value>"
    let needle = "\"f\":\"";
    let start = s.find(needle)? + needle.len();
    let rest = &s[start..];
    let mut result = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => return Some(result),
            '\\' => match chars.next()? {
                '"' => result.push('"'),
                '\\' => result.push('\\'),
                'n' => result.push('\n'),
                'r' => {},
                't' => result.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                        if let Some(c) = char::from_u32(cp) { result.push(c); }
                    }
                },
                c => { result.push('\\'); result.push(c); }
            },
            c => result.push(c),
        }
    }
}

/// Search via SmartSUI server (stealth scraping + AI fact compression).
async fn pico_search_server(query: &str) -> Result<String, String> {
    let body_str = format!(
        r#"{{"query":"{}","mode":"search","max_bytes":4000}}"#,
        json_escape(query)
    );
    let request = HttpRequestArgs {
        url: PICO_SERVER_URL.to_string(),
        method: HttpMethod::POST,
        body: Some(body_str.into_bytes()),
        max_response_bytes: Some(6_000),
        transform: None,
        headers: vec![
            HttpHeader { name: "Content-Type".into(), value: "application/json".into() },
            HttpHeader { name: "X-Api-Key".into(), value: PICO_SERVER_KEY.into() },
        ],
        is_replicated: Some(false),
    };
    bump_metric(|m| m.total_calls += 1);
    let bal_before = ic_cdk::api::canister_cycle_balance();
    let response = mgmt_http_request(&request).await
        .map_err(|e| { bump_metric(|m| m.errors += 1); format!("Server search failed: {:?}", e) })?;
    let bal_after = ic_cdk::api::canister_cycle_balance();
    bump_metric(|m| m.total_cycles_spent += bal_before.saturating_sub(bal_after) as u64);

    extract_intel_facts(&response.body)
        .ok_or_else(|| "No facts in server response".into())
}

/// Scrape via SmartSUI server (Scrapling stealth + AI compression).
async fn pico_browse_server(target_url: &str) -> Result<String, String> {
    let body_str = format!(
        r#"{{"query":"extract content","mode":"browse","url":"{}","max_bytes":3000}}"#,
        json_escape(target_url)
    );
    let request = HttpRequestArgs {
        url: PICO_SERVER_URL.to_string(),
        method: HttpMethod::POST,
        body: Some(body_str.into_bytes()),
        max_response_bytes: Some(5_000),
        transform: None,
        headers: vec![
            HttpHeader { name: "Content-Type".into(), value: "application/json".into() },
            HttpHeader { name: "X-Api-Key".into(), value: PICO_SERVER_KEY.into() },
        ],
        is_replicated: Some(false),
    };
    bump_metric(|m| m.total_calls += 1);
    let bal_before = ic_cdk::api::canister_cycle_balance();
    let response = mgmt_http_request(&request).await
        .map_err(|e| { bump_metric(|m| m.errors += 1); format!("Server browse failed: {:?}", e) })?;
    let bal_after = ic_cdk::api::canister_cycle_balance();
    bump_metric(|m| m.total_cycles_spent += bal_before.saturating_sub(bal_after) as u64);

    extract_intel_facts(&response.body)
        .ok_or_else(|| "No content in server response".into())
}

/// Jina Reader fallback for scraping.
async fn pico_scrape_jina(target_url: &str) -> Result<String, String> {
    let jina_url = format!("https://r.jina.ai/{}", target_url);
    let request = HttpRequestArgs {
        url: jina_url,
        method: HttpMethod::GET,
        body: None,
        max_response_bytes: Some(20_000),
        transform: None,
        headers: vec![
            HttpHeader { name: "Accept".into(), value: "text/plain".into() },
        ],
        is_replicated: Some(false),
    };
    bump_metric(|m| m.total_calls += 1);
    let bal_before = ic_cdk::api::canister_cycle_balance();
    let response = mgmt_http_request(&request).await
        .map_err(|e| { bump_metric(|m| m.errors += 1); format!("Scrape failed: {:?}", e) })?;
    let bal_after = ic_cdk::api::canister_cycle_balance();
    bump_metric(|m| m.total_cycles_spent += bal_before.saturating_sub(bal_after) as u64);

    String::from_utf8(response.body)
        .map_err(|_| "Error decoding scraped content".into())
}

/// Scrape a URL: try server first, fallback to Jina.
async fn pico_scrape(target_url: &str) -> Result<String, String> {
    match pico_browse_server(target_url).await {
        Ok(content) if !content.is_empty() => Ok(content),
        _ => pico_scrape_jina(target_url).await,
    }
}

/// Check if response contains a tool_calls array (AI decided to use a tool).
fn has_tool_call(body: &[u8]) -> bool {
    std::str::from_utf8(body).map(|s| s.contains("\"tool_calls\"")).unwrap_or(false)
}

/// Extract tool_call ID and search query from the LLM response.
/// Returns (tool_call_id, query). Handles string and object argument formats.
fn extract_tool_call(body: &[u8]) -> Option<(String, String)> {
    let s = std::str::from_utf8(body).ok()?;

    // Extract tool_call id (needed for proper tool result message)
    let id = extract_json_string_field(s, "\"id\":")
        .unwrap_or_else(|| "call_0".to_string());

    // Extract arguments (could be string or object)
    let args_needle = "\"arguments\":";
    let args_pos = s.find(args_needle)? + args_needle.len();
    let rest = s[args_pos..].trim_start();

    let args_str = if rest.starts_with('"') {
        // String format: "{\"query\":\"...\"}" — unescape
        let inner = &rest[1..];
        let mut out = String::new();
        let mut chars = inner.chars();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    c => { out.push('\\'); out.push(c); }
                },
                c => out.push(c),
            }
        }
        out
    } else {
        // Raw object format: {"query":"..."} — take until closing }
        let end = rest.find('}').unwrap_or(rest.len());
        rest[..=end].to_string()
    };

    // Try "query":"<value>" and "query": "<value>"
    for needle in &["\"query\":\"", "\"query\": \""] {
        if let Some(qstart) = args_str.find(needle) {
            let after = &args_str[qstart + needle.len()..];
            let qend = after.find('"').unwrap_or(after.len());
            let q = &after[..qend];
            if !q.is_empty() { return Some((id, q.to_string())); }
        }
    }
    None
}

/// Extract a simple "key":"value" string field from JSON.
fn extract_json_string_field(s: &str, needle: &str) -> Option<String> {
    let pos = s.find(needle)? + needle.len();
    let rest = s[pos..].trim_start();
    if !rest.starts_with('"') { return None; }
    let inner = &rest[1..];
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}


/// Detect if the AI refused to search and told the user to check a website instead.
fn is_search_refusal(reply: &str) -> bool {
    let lower = reply.to_lowercase();
    // Must match at least one refusal pattern
    let refusal = lower.contains("i can't browse")
        || lower.contains("i cannot browse")
        || lower.contains("i can't access")
        || lower.contains("i cannot access")
        || lower.contains("i don't have access to real-time")
        || lower.contains("i don't have the ability to browse")
        || lower.contains("check a reliable news")
        || lower.contains("check a news website")
        || lower.contains("recommend checking")
        || lower.contains("visit a website")
        || lower.contains("i'm unable to fetch")
        || lower.contains("i'm unable to browse")
        || lower.contains("i can't fetch")
        || lower.contains("cannot fetch the latest")
        || lower.contains("don't have real-time")
        || lower.contains("no real-time access");
    refusal
}

/// Search via SmartSUI server first, fallback to Google News RSS.
async fn pico_search(query: &str) -> Result<String, String> {
    match pico_search_server(query).await {
        Ok(facts) if !facts.is_empty() && facts.len() > 20 => Ok(facts),
        _ => pico_search_rss(query).await,
    }
}

/// Google News RSS fallback search.
async fn pico_search_rss(query: &str) -> Result<String, String> {
    let encoded: String = query.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
            c.to_string()
        } else if c == ' ' {
            "+".to_string()
        } else {
            format!("%{:02X}", c as u32)
        }
    }).collect();
    let search_url = format!(
        "https://news.google.com/rss/search?q={}&hl=en-US&gl=US&ceid=US:en", encoded
    );
    let request = HttpRequestArgs {
        url: search_url,
        method: HttpMethod::GET,
        body: None,
        max_response_bytes: Some(2_000_000),
        transform: None,
        headers: vec![],
        is_replicated: Some(false),
    };
    bump_metric(|m| m.total_calls += 1);
    let bal_before = ic_cdk::api::canister_cycle_balance();
    let response = mgmt_http_request(&request).await
        .map_err(|e| { bump_metric(|m| m.errors += 1); format!("Search failed: {:?}", e) })?;
    let bal_after = ic_cdk::api::canister_cycle_balance();
    bump_metric(|m| m.total_cycles_spent += bal_before.saturating_sub(bal_after) as u64);

    let xml = String::from_utf8(response.body)
        .map_err(|_| String::from("Error decoding search results"))?;
    let mut results = String::with_capacity(2000);
    let mut count = 0u8;
    let mut pos = 0usize;
    while let Some(start) = xml[pos..].find("<title>") {
        let abs_start = pos + start + 7;
        if let Some(end) = xml[abs_start..].find("</title>") {
            let title = &xml[abs_start..abs_start + end];
            pos = abs_start + end + 8;
            count += 1;
            if count <= 2 { continue; }
            if count > 12 { break; }
            results.push_str(&format!("{}. {}\n", count - 2, title));
        } else {
            break;
        }
    }
    if results.is_empty() { results.push_str("No results found."); }
    Ok(results)
}

fn store_web_entry(url: &str, content: &str) {
    let idx = WEB_COUNTER.with(|c| {
        let mut cell = c.borrow_mut();
        let count = cell.get().clone();
        let _ = cell.set(count + 1);
        (count % 12) as u8
    });
    let summary: String = content.chars().take(300).collect();
    let entry = WebEntry {
        url: url.to_string(),
        summary,
        timestamp: ic_cdk::api::time(),
    };
    WEB_MEM.with(|m| m.borrow_mut().insert(idx, entry));
}

/// Build the ultra-compressed messages array.  Exactly 2-3 JSON messages:
///   1. system prompt + structured PicoState (I:/T:/E:/P: tiers)
///   2. last assistant reply, truncated (for reference continuity) — optional
///   3. current user prompt
fn build_messages_json(config: &AgentConfig, prompt: &str) -> String {
    let mut json = String::with_capacity(4096);
    json.push('[');

    // ── message 1: system prompt + tiered PicoState ──
    let state = SESSION_NOTES.with(|s| s.borrow().get().clone());
    let profile = USER_PROFILE.with(|p| p.borrow().get().clone());
    // Inject custom name: replace "PicoClaw" in system prompt with user's chosen name
    let sys_prompt = if profile.name != "PicoClaw" && !profile.name.is_empty() {
        config.system_prompt.replace("PicoClaw", &profile.name)
    } else {
        config.system_prompt.clone()
    };
    json.push_str("{\"role\":\"system\",\"content\":\"");
    json.push_str(&json_escape(&sys_prompt));

    let has_state = !state.identity.is_empty() || !state.thread.is_empty()
        || !state.episodes.is_empty() || !state.priors.is_empty();
    if has_state {
        json.push_str("\\n\\n[M]\\n");
        if !state.identity.is_empty() {
            json.push_str("I:");
            json.push_str(&json_escape(&state.identity));
            json.push_str("\\n");
        }
        if !state.thread.is_empty() {
            json.push_str("T:");
            json.push_str(&json_escape(&state.thread));
            json.push_str("\\n");
        }
        if !state.episodes.is_empty() {
            json.push_str("E:");
            json.push_str(&json_escape(&state.episodes));
            json.push_str("\\n");
        }
        if !state.priors.is_empty() {
            json.push_str("P:");
            json.push_str(&json_escape(&state.priors));
        }
    }

    // ── [W] web memory summaries ──
    let web_entries: Vec<WebEntry> = WEB_MEM.with(|m| {
        let map = m.borrow();
        let mut entries: Vec<WebEntry> = (0u8..12).filter_map(|i| map.get(&i)).collect();
        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        entries
    });
    if !web_entries.is_empty() {
        json.push_str("\\n\\n[W] Recent lookups:\\n");
        let now = ic_cdk::api::time();
        for (i, entry) in web_entries.iter().enumerate() {
            let ago_secs = (now.saturating_sub(entry.timestamp)) / 1_000_000_000;
            let ago = if ago_secs < 60 { format!("{}s ago", ago_secs) }
                else if ago_secs < 3600 { format!("{}m ago", ago_secs / 60) }
                else { format!("{}h ago", ago_secs / 3600) };
            let preview: String = entry.summary.chars().take(100).collect();
            json.push_str(&format!("{}. ", i + 1));
            json.push_str(&json_escape(&entry.url));
            json.push_str(" (");
            json.push_str(&ago);
            json.push_str("): ");
            json.push_str(&json_escape(&preview));
            json.push_str("\\n");
        }
    }

    json.push_str("\"}");

    // ── message 2 (optional): last assistant reply, truncated for continuity ──
    if config.max_context_messages > 0 {
        let counter = MSG_COUNTER.with(|c| *c.borrow());
        let last_asst: Option<String> = CHAT_LOG.with(|c| {
            let map = c.borrow();
            let floor = counter.saturating_sub(4);
            for id in (floor..counter).rev() {
                if let Some(msg) = map.get(&id) {
                    if msg.role == "assistant" {
                        return Some(msg.content.clone());
                    }
                }
            }
            None
        });

        if let Some(content) = last_asst {
            let truncated = truncate_utf8(&content, LAST_REPLY_MAX_CHARS);
            json.push_str(",{\"role\":\"assistant\",\"content\":\"");
            json.push_str(&json_escape(truncated));
            if content.len() > LAST_REPLY_MAX_CHARS {
                json.push_str("...");
            }
            json.push_str("\"}");
        }
    }

    // ── message 3: current user prompt ──
    json.push_str(",{\"role\":\"user\",\"content\":\"");
    json.push_str(&json_escape(prompt));
    json.push_str("\"}");

    json.push(']');
    json
}

const TOOLS_JSON: &str = r#","tools":[{"type":"function","function":{"name":"web_search","description":"Search the web for current information: news, prices, weather, sports, facts, or anything you need real-time data for. Always use this instead of saying you cannot browse.","parameters":{"type":"object","properties":{"query":{"type":"string","description":"Search query"}},"required":["query"]}}}],"tool_choice":"auto""#;

fn build_request_body(config: &AgentConfig, prompt: &str) -> Vec<u8> {
    build_request_body_inner(config, prompt, true)
}

fn build_request_body_no_tools(config: &AgentConfig, prompt: &str) -> Vec<u8> {
    build_request_body_inner(config, prompt, false)
}


fn build_request_body_inner(config: &AgentConfig, prompt: &str, with_tools: bool) -> Vec<u8> {
    let messages = build_messages_json(config, prompt);
    let mut body = String::with_capacity(messages.len() + 512);
    body.push_str("{\"model\":\"");
    body.push_str(&json_escape(&config.model));
    body.push_str("\",\"messages\":");
    body.push_str(&messages);
    body.push_str(",\"temperature\":0.7,\"max_tokens\":2048");
    if with_tools { body.push_str(TOOLS_JSON); }
    body.push('}');
    body.into_bytes()
}

/// Build a raw JSON request body for an arbitrary messages array (used by compress).
fn build_raw_request_body(config: &AgentConfig, messages_json: &str) -> Vec<u8> {
    let mut body = String::with_capacity(messages_json.len() + 128);
    body.push_str("{\"model\":\"");
    body.push_str(&json_escape(&config.model));
    body.push_str("\",\"messages\":");
    body.push_str(messages_json);
    body.push_str(",\"temperature\":0.3,\"max_tokens\":640}");
    body.into_bytes()
}

/// Check whether automatic compression should run.
fn should_compress(config: &AgentConfig) -> bool {
    if config.compress_interval == 0 {
        return false;
    }
    let counter = MSG_COUNTER.with(|c| *c.borrow());
    let last_compressed = SESSION_NOTES.with(|s| s.borrow().get().msg_id_at_compress);
    let msgs_since = counter.saturating_sub(last_compressed);
    msgs_since >= config.compress_interval as u64
}

/// Compress recent conversation into PicoState tiers via a non-replicated Chutes LLM call.
/// LLM outputs I:/T:/E: lines; canister parses and stores per-tier.
/// Priors (P:) are preserved — they're Wasm-managed, not LLM-managed.
async fn run_compression() -> Result<(), String> {
    let config = get_config();
    let api_key = get_api_key().await
        .ok_or("API key not configured")?;

    let counter = MSG_COUNTER.with(|c| *c.borrow());
    let state = SESSION_NOTES.with(|s| s.borrow().get().clone());
    let last_compressed = state.msg_id_at_compress;

    let recent: Vec<Message> = CHAT_LOG.with(|c| {
        let map = c.borrow();
        map.range(last_compressed + 1..=counter).map(|(_, m)| m).collect()
    });

    if recent.is_empty() {
        return Ok(());
    }

    // Build truncated transcript — each message capped to save bytes
    let mut transcript = String::with_capacity(recent.len() * (TRANSCRIPT_MSG_MAX_CHARS + 8));
    for msg in &recent {
        transcript.push_str(if msg.role == "assistant" { "A:" } else { "U:" });
        let t = truncate_utf8(&msg.content, TRANSCRIPT_MSG_MAX_CHARS);
        transcript.push_str(t);
        if msg.content.len() > TRANSCRIPT_MSG_MAX_CHARS {
            transcript.push_str("..");
        }
        transcript.push('\n');
    }

    // User prompt: existing tiers + separator + new transcript
    let mut compress_prompt = String::with_capacity(
        state.identity.len() + state.thread.len() + state.episodes.len()
        + transcript.len() + 64
    );
    compress_prompt.push_str("I:");
    compress_prompt.push_str(&state.identity);
    compress_prompt.push_str("\nT:");
    compress_prompt.push_str(&state.thread);
    compress_prompt.push_str("\nE:");
    compress_prompt.push_str(&state.episodes);
    compress_prompt.push_str("\n---\n");
    compress_prompt.push_str(&transcript);

    // Multi-tier compression system instruction
    let sys = "You maintain 3 memory tiers. Above: current I/T/E state + new messages after ---.\n\
Output EXACTLY 3 lines:\n\
I: key=val|key=val — permanent facts (name,project,tech,prefs). Keep ALL existing keys. Add/update ONLY from new info.\n\
T: telegram-style current thread, max 580 chars. REPLACE old thread with latest focus.\n\
E: rolling episode log. IF topic changed: prepend 1-line old-thread archive to existing list; drop oldest if >880ch. IF same topic: keep existing E unchanged.\n\
Rules: no articles, no filler, pipe-delimit facts, abbreviate aggressively. ONLY output I:/T:/E: lines.";

    let mut messages_json = String::with_capacity(compress_prompt.len() + 768);
    messages_json.push_str("[{\"role\":\"system\",\"content\":\"");
    messages_json.push_str(&json_escape(sys));
    messages_json.push_str("\"},{\"role\":\"user\",\"content\":\"");
    messages_json.push_str(&json_escape(&compress_prompt));
    messages_json.push_str("\"}]");

    let body = build_raw_request_body(&config, &messages_json);

    let request = HttpRequestArgs {
        url: config.api_endpoint.clone(),
        max_response_bytes: Some(3072),
        method: HttpMethod::POST,
        headers: vec![
            HttpHeader { name: "Content-Type".into(), value: "application/json".into() },
            HttpHeader { name: "Authorization".into(), value: format!("Bearer {}", api_key) },
        ],
        body: Some(body),
        transform: None,
        is_replicated: Some(false),
    };

    bump_metric(|m| m.total_calls += 1);
    let bal_before = ic_cdk::api::canister_cycle_balance();

    let response = mgmt_http_request(&request).await
        .map_err(|e| {
            bump_metric(|m| m.errors += 1);
            format!("Compression outcall failed: {:?}", e)
        })?;

    let bal_after = ic_cdk::api::canister_cycle_balance();
    let actual_spent = bal_before.saturating_sub(bal_after) as u64;
    bump_metric(|m| m.total_cycles_spent += actual_spent);

    // Check HTTP status
    let status = response.status.0.to_u64_digits();
    let status_code = if status.is_empty() { 0u64 } else { status[0] };
    if status_code < 200 || status_code >= 300 {
        let body_str = String::from_utf8_lossy(&response.body);
        bump_metric(|m| m.errors += 1);
        return Err(format!("Compression API error ({}): {}", status_code, body_str));
    }

    let raw = extract_content(&response.body)
        .unwrap_or_else(|| String::from_utf8_lossy(&response.body).into_owned());

    if raw.is_empty() {
        bump_metric(|m| m.errors += 1);
        return Err("Empty response from LLM compression".into());
    }

    let (new_i, new_t, new_e) = parse_tiers(&raw);

    // Robust fallback: if parser got nothing, treat raw as thread, keep existing I/E
    let (identity, thread, episodes) = if new_i.is_empty() && new_t.is_empty() && new_e.is_empty() {
        (state.identity.clone(),
         truncate_utf8(&raw, MAX_THREAD_CHARS).to_string(),
         state.episodes.clone())
    } else {
        (if new_i.is_empty() { state.identity.clone() }
            else { truncate_utf8(&new_i, MAX_IDENTITY_CHARS).to_string() },
         truncate_utf8(&new_t, MAX_THREAD_CHARS).to_string(),
         if new_e.is_empty() { state.episodes.clone() }
            else { truncate_utf8(&new_e, MAX_EPISODES_CHARS).to_string() })
    };

    SESSION_NOTES.with(|s| {
        let _ = s.borrow_mut().set(PicoState {
            identity,
            thread,
            episodes,
            priors: state.priors.clone(), // preserve Wasm-managed priors
            updated_at: ic_cdk::api::time(),
            msg_id_at_compress: counter,
        });
    });

    Ok(())
}


// ═══════════════════════════════════════════════════════════════════════
//  On-chain tools (free query calls — zero cycles)
// ═══════════════════════════════════════════════════════════════════════

/// Minimal SHA-224 — pure Wasm, no dependencies, ~40 lines.
fn sha224(data: &[u8]) -> [u8; 28] {
    const K: [u32; 64] = [
        0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
        0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
        0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
        0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
        0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
        0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
        0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
        0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0xc1059ed8, 0x367cd507, 0x3070dd17, 0xf70e5939,
        0xffc00b31, 0x68581511, 0x64f98fa7, 0xbefa4fa4,
    ];
    // Pad: append 0x80, zeros, then 64-bit big-endian bit length
    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 { padded.push(0); }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    // Process 512-bit blocks
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i*4..i*4+4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i-15].rotate_right(7) ^ w[i-15].rotate_right(18) ^ (w[i-15] >> 3);
            let s1 = w[i-2].rotate_right(17) ^ w[i-2].rotate_right(19) ^ (w[i-2] >> 10);
            w[i] = w[i-16].wrapping_add(s0).wrapping_add(w[i-7]).wrapping_add(s1);
        }
        let [mut a,mut b,mut c,mut d,mut e,mut f,mut g,mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e; e = d.wrapping_add(t1);
            d = c; c = b; b = a; a = t1.wrapping_add(t2);
        }
        for (i, v) in [a,b,c,d,e,f,g,hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    // SHA-224 = first 28 bytes of SHA-256 state (7 words)
    let mut out = [0u8; 28];
    for i in 0..7 { out[i*4..i*4+4].copy_from_slice(&h[i].to_be_bytes()); }
    out
}

/// CRC-32 (ISO 3309) — table-less, compact.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB88320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Convert a Principal to an ICP Account ID (default subaccount).
/// Formula: CRC32(SHA-224("\x0Aaccount-id" + principal_bytes + 32_zero_bytes))
/// Returns 64-char hex string.
fn derive_account_id(principal: &Principal) -> String {
    let mut hasher_input = Vec::with_capacity(64);
    hasher_input.extend_from_slice(b"\x0Aaccount-id");
    hasher_input.extend_from_slice(principal.as_slice());
    hasher_input.extend_from_slice(&[0u8; 32]); // default subaccount
    let hash = sha224(&hasher_input);
    let checksum = crc32(&hash);
    let mut hex = String::with_capacity(64);
    for b in checksum.to_be_bytes().iter().chain(hash.iter()) {
        let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{:02x}", b));
    }
    hex
}

/// Free query: Principal text → Account ID hex. Zero cycles.
#[ic_cdk::query]
fn principal_to_account_id(principal_text: String) -> Result<String, String> {
    let principal = Principal::from_text(&principal_text)
        .map_err(|e| format!("Invalid principal: {}", e))?;
    Ok(derive_account_id(&principal))
}

// ═══════════════════════════════════════════════════════════════════════
//  User profile endpoints
// ═══════════════════════════════════════════════════════════════════════

#[ic_cdk::update]
fn set_profile(name: String, avatar_url: String) -> Result<(), String> {
    require_authorized()?;
    if name.len() > 32 {
        return Err("Name too long (max 32 chars)".into());
    }
    if avatar_url.len() > 256 {
        return Err("Avatar URL too long (max 256 chars)".into());
    }
    if !avatar_url.is_empty() && !avatar_url.starts_with("http") {
        return Err("Avatar URL must start with http".into());
    }
    USER_PROFILE.with(|p| {
        let _ = p.borrow_mut().set(UserProfile {
            name: if name.is_empty() { "PicoClaw".into() } else { name },
            avatar_url,
            updated_at: ic_cdk::api::time(),
        });
    });
    Ok(())
}

#[ic_cdk::query]
fn get_profile() -> UserProfile {
    require_authorized().unwrap_or_else(|_| ic_cdk::trap("Access denied"));
    USER_PROFILE.with(|p| p.borrow().get().clone())
}

// ═══════════════════════════════════════════════════════════════════════
//  Admin endpoints
// ═══════════════════════════════════════════════════════════════════════

#[ic_cdk::update]
async fn set_api_key(key: String) -> Result<(), String> {
    require_controller()?;

    // Derive VetKey for encryption
    let vk = get_or_derive_vetkey().await?;

    // Generate random nonce
    let rand = ic_cdk::management_canister::raw_rand()
        .await
        .map_err(|e| format!("raw_rand failed: {:?}", e))?;
    let mut nonce = [0u8; ENC_NONCE_LEN];
    nonce.copy_from_slice(&rand[..ENC_NONCE_LEN]);

    // Encrypt the API key
    let ciphertext = xor_with_keystream(&vk, &nonce, key.as_bytes());

    // Store: magic || nonce || ciphertext
    let mut stored = Vec::with_capacity(ENC_MAGIC.len() + ENC_NONCE_LEN + ciphertext.len());
    stored.extend_from_slice(&ENC_MAGIC);
    stored.extend_from_slice(&nonce);
    stored.extend_from_slice(&ciphertext);
    API_KEY_STORE.with(|k| { let _ = k.borrow_mut().set(SecretString(stored)); });

    // Clear any legacy key that may still be in the config cell
    CONFIG.with(|c| {
        let mut cell = c.borrow_mut();
        let mut cfg = cell.get().clone();
        if cfg.api_key.is_some() {
            cfg.api_key = None;
            let _ = cell.set(cfg);
        }
    });
    Ok(())
}

#[ic_cdk::update]
fn configure(config: AgentConfig) -> Result<(), String> {
    require_controller()?;
    // Never allow the API key to be set via configure — use set_api_key instead
    let mut clean = config;
    clean.api_key = None;
    CONFIG.with(|c| { let _ = c.borrow_mut().set(clean); });
    Ok(())
}

#[ic_cdk::query]
fn get_config_public() -> AgentConfig {
    CONFIG.with(|c| {
        let mut cfg = c.borrow().get().clone();
        // Never expose the API key — always return None
        cfg.api_key = None;
        cfg
    })
}

// ═══════════════════════════════════════════════════════════════════════
//  Core LLM interaction
// ═══════════════════════════════════════════════════════════════════════

/// Dispatch a dev task to the Hetzner agent via HTTP outcall.
async fn dispatch_dev_task(task_prompt: &str) -> Result<String, String> {
    let body_str = format!(
        r#"{{"repo":"{}","prompt":"{}"}}"#,
        DEV_DEFAULT_REPO,
        json_escape(task_prompt)
    );
    let request = HttpRequestArgs {
        url: DEV_AGENT_URL.to_string(),
        method: HttpMethod::POST,
        body: Some(body_str.into_bytes()),
        max_response_bytes: Some(1_000),
        transform: None,
        headers: vec![
            HttpHeader { name: "Content-Type".into(), value: "application/json".into() },
        ],
        is_replicated: Some(false),
    };
    let response = mgmt_http_request(&request).await
        .map_err(|e| format!("Dev agent unreachable: {:?}", e))?;
    let body = String::from_utf8_lossy(&response.body);
    if body.contains("\"queued\":true") {
        Ok(format!("Dev task dispatched. The agent is working on: {}", task_prompt))
    } else {
        Err(format!("Dev agent error: {}", body))
    }
}

#[ic_cdk::update]
async fn chat(prompt: String) -> Result<String, String> {
    require_authorized()?;

    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!("Prompt too large: {} bytes (max {})", prompt.len(), MAX_PROMPT_BYTES));
    }

    // /dev command → dispatch to Hetzner dev agent, skip LLM
    if prompt.starts_with("/dev ") {
        let task = &prompt[5..];
        log_message("user", &prompt);
        let reply = match dispatch_dev_task(task).await {
            Ok(msg) => msg,
            Err(e) => format!("Failed to dispatch dev task: {}", e),
        };
        log_message("assistant", &reply);
        return Ok(reply);
    }

    let config = get_config();
    let api_key = get_api_key().await
        .ok_or("API key not configured")?;

    log_message("user", &prompt);

    // URL in user message? Auto-scrape via Jina Reader before LLM call
    let mut augmented_prompt = prompt.clone();
    if let Some(url) = extract_url(&prompt) {
        let url_owned = url.to_string();
        match pico_scrape(&url_owned).await {
            Ok(content) => {
                store_web_entry(&url_owned, &content);
                let truncated: String = content.chars().take(6000).collect();
                augmented_prompt = format!("{}\n\n[Web: {}]\n{}", prompt, url_owned, truncated);
            }
            Err(e) => {
                augmented_prompt = format!("{}\n\n[Web scrape failed: {}]", prompt, e);
            }
        }
    }

    let body = build_request_body(&config, &augmented_prompt);

    // Non-replicated outcall: only 1 subnet node makes the request (no consensus needed)
    let request = HttpRequestArgs {
        url: config.api_endpoint.clone(),
        max_response_bytes: Some(config.max_response_bytes),
        method: HttpMethod::POST,
        headers: vec![
            HttpHeader { name: "Content-Type".into(), value: "application/json".into() },
            HttpHeader { name: "Authorization".into(), value: format!("Bearer {}", api_key) },
        ],
        body: Some(body),
        transform: None,
        is_replicated: Some(false),
    };

    bump_metric(|m| m.total_calls += 1);
    let bal_before = ic_cdk::api::canister_cycle_balance();

    let response = mgmt_http_request(&request).await
        .map_err(|e| {
            bump_metric(|m| m.errors += 1);
            format!("HTTP outcall failed: {:?}", e)
        })?;

    let bal_after = ic_cdk::api::canister_cycle_balance();
    let actual_spent = bal_before.saturating_sub(bal_after) as u64;
    bump_metric(|m| m.total_cycles_spent += actual_spent);

    // Check HTTP status
    let status = response.status.0.to_u64_digits();
    let status_code = if status.is_empty() { 0u64 } else { status[0] };
    if status_code < 200 || status_code >= 300 {
        let body_str = String::from_utf8_lossy(&response.body);
        bump_metric(|m| m.errors += 1);
        return Err(format!("API error ({}): {}", status_code, body_str));
    }

    // ── Tool loop: detect tool_calls → execute → re-call with result ──
    let reply;
    if has_tool_call(&response.body) {
        // Extract search query from tool call; fallback = user's original prompt
        let query = extract_tool_call(&response.body)
            .map(|(_, q)| q)
            .unwrap_or_else(|| prompt.clone());

        // Execute search
        let tool_result = match pico_search(&query).await {
            Ok(results) => {
                let label: String = query.chars().take(60).collect();
                store_web_entry(&format!("search: {}", label), &results);
                results.chars().take(6000).collect::<String>()
            }
            Err(e) => format!("Search failed: {}", e),
        };

        // Re-call LLM with search results injected into user prompt (no tools).
        // Note: proper tool_calls→tool message flow fails on Chutes/DeepSeek,
        // so we use the simpler approach of augmenting the user message.
        let search_prompt = format!("{}\n\n[Search results for: {}]\n{}", augmented_prompt, query, tool_result);
        let body2 = build_request_body_no_tools(&config, &search_prompt);
        let req2 = HttpRequestArgs {
            url: config.api_endpoint.clone(),
            max_response_bytes: Some(config.max_response_bytes),
            method: HttpMethod::POST,
            headers: vec![
                HttpHeader { name: "Content-Type".into(), value: "application/json".into() },
                HttpHeader { name: "Authorization".into(), value: format!("Bearer {}", api_key) },
            ],
            body: Some(body2),
            transform: None,
            is_replicated: Some(false),
        };
        bump_metric(|m| m.total_calls += 1);
        let b2 = ic_cdk::api::canister_cycle_balance();
        let resp2 = mgmt_http_request(&req2).await
            .map_err(|e| { bump_metric(|m| m.errors += 1); format!("Search follow-up failed: {:?}", e) })?;
        let b3 = ic_cdk::api::canister_cycle_balance();
        bump_metric(|m| m.total_cycles_spent += b2.saturating_sub(b3) as u64);
        reply = extract_content(&resp2.body)
            .unwrap_or_else(|| "Search completed but could not parse follow-up".into());
    } else {
        reply = extract_content(&response.body).ok_or_else(|| {
            bump_metric(|m| m.errors += 1);
            let resp_str = std::str::from_utf8(&response.body).unwrap_or("");
            let snippet: String = resp_str.chars().take(300).collect();
            format!("Failed to parse LLM response: {}", snippet)
        })?;
    }

    if reply.is_empty() {
        bump_metric(|m| m.errors += 1);
        return Err("Empty response from LLM".into());
    }

    // Refusal detection: if AI refused to search and told user to check a website,
    // force a search with the user's original prompt and re-call
    let reply = if is_search_refusal(&reply) {
        let query = prompt.clone();
        match pico_search(&query).await {
            Ok(results) => {
                let label: String = query.chars().take(60).collect();
                store_web_entry(&format!("search: {}", label), &results);
                let truncated: String = results.chars().take(6000).collect();
                let search_prompt = format!(
                    "{}\n\n[Search results for: {}]\n{}", prompt, query, truncated
                );
                let body2 = build_request_body_no_tools(&config, &search_prompt);
                let req2 = HttpRequestArgs {
                    url: config.api_endpoint.clone(),
                    max_response_bytes: Some(config.max_response_bytes),
                    method: HttpMethod::POST,
                    headers: vec![
                        HttpHeader { name: "Content-Type".into(), value: "application/json".into() },
                        HttpHeader { name: "Authorization".into(), value: format!("Bearer {}", api_key) },
                    ],
                    body: Some(body2),
                    transform: None,
                    is_replicated: Some(false),
                };
                bump_metric(|m| m.total_calls += 1);
                let b2 = ic_cdk::api::canister_cycle_balance();
                let resp2 = mgmt_http_request(&req2).await
                    .map_err(|e| { bump_metric(|m| m.errors += 1); format!("Forced search failed: {:?}", e) })?;
                let b3 = ic_cdk::api::canister_cycle_balance();
                bump_metric(|m| m.total_cycles_spent += b2.saturating_sub(b3) as u64);
                extract_content(&resp2.body).unwrap_or(reply)
            }
            Err(_) => reply, // search failed, return original reply
        }
    } else {
        reply
    };

    log_message("assistant", &reply);

    if should_compress(&config) {
        ic_cdk::futures::spawn(async {
            let _ = run_compression().await;
        });
    }

    Ok(reply)
}

/// Backward-compatible alias.
#[ic_cdk::update]
async fn send_prompt_to_llm(prompt: String) -> Result<String, String> {
    chat(prompt).await
}

/// No-op transform — kept for backward compatibility with .did file.
/// Non-replicated outcalls don't need transforms, but the .did declares this.
#[ic_cdk::query]
fn transform_llm_response(raw: TransformArgs) -> HttpRequestResult {
    raw.response
}

/// TransformArgs for the .did-declared transform callback.
#[derive(CandidType, Deserialize)]
pub struct TransformArgs {
    pub response: HttpRequestResult,
    pub context: Vec<u8>,
}

// ═══════════════════════════════════════════════════════════════════════
//  Conversation management
// ═══════════════════════════════════════════════════════════════════════

#[ic_cdk::query]
fn get_history(limit: u64) -> Vec<Message> {
    require_authorized().unwrap_or_else(|_| ic_cdk::trap("Access denied"));
    let counter = MSG_COUNTER.with(|c| *c.borrow());
    CHAT_LOG.with(|c| {
        let map = c.borrow();
        let start = counter.saturating_sub(limit.saturating_sub(1));
        map.range(start..=counter).map(|(_, m)| m).collect()
    })
}

#[ic_cdk::update]
fn clear_history() -> Result<u64, String> {
    require_controller()?;
    let count = CHAT_LOG.with(|c| {
        let mut map = c.borrow_mut();
        let keys: Vec<u64> = map.iter().map(|(k, _)| k).collect();
        let n = keys.len() as u64;
        for k in keys {
            map.remove(&k);
        }
        n
    });
    MSG_COUNTER.with(|c| *c.borrow_mut() = 0);
    Ok(count)
}

// ═══════════════════════════════════════════════════════════════════════
//  Session notes management
// ═══════════════════════════════════════════════════════════════════════

#[ic_cdk::query]
fn get_notes() -> PicoState {
    require_authorized().unwrap_or_else(|_| ic_cdk::trap("Access denied"));
    SESSION_NOTES.with(|s| s.borrow().get().clone())
}

#[ic_cdk::update]
fn clear_notes() -> Result<(), String> {
    require_controller()?;
    SESSION_NOTES.with(|s| {
        let _ = s.borrow_mut().set(PicoState::default());
    });
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
//  Web memory endpoints
// ═══════════════════════════════════════════════════════════════════════

#[ic_cdk::update]
async fn browse(url: String) -> Result<String, String> {
    require_authorized()?;
    let content = pico_scrape(&url).await?;
    store_web_entry(&url, &content);
    Ok(content.chars().take(500).collect())
}

#[ic_cdk::query]
fn get_web_memory() -> Vec<WebEntry> {
    require_authorized().unwrap_or_else(|_| ic_cdk::trap("Access denied"));
    WEB_MEM.with(|m| {
        let map = m.borrow();
        let mut entries: Vec<WebEntry> = (0u8..12).filter_map(|i| map.get(&i)).collect();
        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        entries
    })
}

#[ic_cdk::update]
fn clear_web_memory() -> Result<(), String> {
    require_controller()?;
    WEB_MEM.with(|m| {
        let mut map = m.borrow_mut();
        for i in 0u8..12 { let _ = map.remove(&i); }
    });
    Ok(())
}

/// Manually trigger context compression.
#[ic_cdk::update]
async fn compress_context() -> Result<String, String> {
    require_controller()?;
    run_compression().await?;
    let state = SESSION_NOTES.with(|s| s.borrow().get().clone());
    Ok(format!("I:{}\nT:{}\nE:{}\nP:{}", state.identity, state.thread, state.episodes, state.priors))
}

// ═══════════════════════════════════════════════════════════════════════
//  Monitoring
// ═══════════════════════════════════════════════════════════════════════

#[ic_cdk::query]
fn get_metrics() -> Metrics {
    METRICS_STORE.with(|m| m.borrow().get().clone())
}

#[ic_cdk::query]
fn cycle_balance() -> u128 {
    ic_cdk::api::canister_cycle_balance()
}

// ═══════════════════════════════════════════════════════════════════════
//  Background task queue
// ═══════════════════════════════════════════════════════════════════════

fn next_task_id() -> u64 {
    TASK_COUNTER.with(|c| {
        let mut id = c.borrow_mut();
        *id += 1;
        *id
    })
}

fn enqueue_task(prompt: String) -> u64 {
    let id = next_task_id();
    TASK_QUEUE.with(|q| {
        q.borrow_mut().insert(id, QueuedTask {
            prompt,
            caller: ic_cdk::api::msg_caller(),
            created_at: ic_cdk::api::time(),
        });
    });

    // Fire-and-forget background processing
    ic_cdk::futures::spawn(process_next_task());

    id
}

async fn process_next_task() {
    let task = TASK_QUEUE.with(|q| {
        q.borrow().iter().next().map(|(k, v)| (k, v))
    });

    if let Some((id, task)) = task {
        let _ = chat(task.prompt).await;
        TASK_QUEUE.with(|q| q.borrow_mut().remove(&id));

        // If more tasks remain, schedule another round
        let more = TASK_QUEUE.with(|q| q.borrow().len() > 0);
        if more {
            ic_cdk::futures::spawn(process_next_task());
        }
    }
}

#[ic_cdk::query]
fn get_queue_length() -> u64 {
    TASK_QUEUE.with(|q| q.borrow().len())
}

// ═══════════════════════════════════════════════════════════════════════
//  HTTP Gateway — serves a lightweight REST API
// ═══════════════════════════════════════════════════════════════════════

#[derive(CandidType, Deserialize)]
pub struct IngressHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(CandidType, Deserialize)]
pub struct IngressHttpResponse {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub upgrade: Option<bool>,
}

fn json_response(status: u16, body: &str) -> IngressHttpResponse {
    IngressHttpResponse {
        status_code: status,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Access-Control-Allow-Origin".into(), "*".into()),
        ],
        body: body.as_bytes().to_vec(),
        upgrade: None,
    }
}

fn get_path(url: &str) -> &str {
    url.split('?').next().unwrap_or("/")
}

#[ic_cdk::query]
fn http_request(req: IngressHttpRequest) -> IngressHttpResponse {
    // Upgrade POSTs to update calls
    if req.method == "POST" {
        return IngressHttpResponse {
            status_code: 200,
            headers: vec![],
            body: vec![],
            upgrade: Some(true),
        };
    }

    match get_path(&req.url) {
        "/" | "/health" => json_response(200,
            "{\"status\":\"ok\",\"canister\":\"picoclaw\",\"version\":\"0.2.0\"}"
        ),

        "/metrics" => {
            let m = METRICS_STORE.with(|s| s.borrow().get().clone());
            let bal = ic_cdk::api::canister_cycle_balance();
            let mut body = String::with_capacity(128);
            body.push_str("{\"total_calls\":");
            body.push_str(&m.total_calls.to_string());
            body.push_str(",\"total_messages\":");
            body.push_str(&m.total_messages.to_string());
            body.push_str(",\"errors\":");
            body.push_str(&m.errors.to_string());
            body.push_str(",\"cycle_balance\":");
            body.push_str(&bal.to_string());
            body.push_str(",\"queue_depth\":");
            body.push_str(&TASK_QUEUE.with(|q| q.borrow().len()).to_string());
            body.push('}');
            json_response(200, &body)
        }

        // /history and /config removed — use authenticated canister calls instead.
        _ => json_response(404, "{\"error\":\"not found\"}"),
    }
}

#[ic_cdk::update]
async fn http_request_update(req: IngressHttpRequest) -> IngressHttpResponse {
    if req.method != "POST" {
        return json_response(405, "{\"error\":\"method not allowed\"}");
    }

    // HTTP gateway calls come from the anonymous principal — reject them.
    // Use native canister calls with Internet Identity authentication instead.
    if ic_cdk::api::msg_caller() == Principal::anonymous() {
        return json_response(403, "{\"error\":\"anonymous HTTP calls disabled — use authenticated canister calls\"}");
    }

    match get_path(&req.url) {
        "/chat" => {
            let prompt = extract_prompt(&req.body)
                .unwrap_or_else(|| String::from_utf8_lossy(&req.body).into_owned());

            match chat(prompt).await {
                Ok(reply) => {
                    let mut body = String::with_capacity(reply.len() + 32);
                    body.push_str("{\"response\":\"");
                    body.push_str(&json_escape(&reply));
                    body.push_str("\"}");
                    json_response(200, &body)
                }
                Err(e) => {
                    let mut body = String::with_capacity(e.len() + 24);
                    body.push_str("{\"error\":\"");
                    body.push_str(&json_escape(&e));
                    body.push_str("\"}");
                    json_response(500, &body)
                }
            }
        }

        "/webhook" => {
            let prompt = extract_prompt(&req.body)
                .unwrap_or_else(|| String::from_utf8_lossy(&req.body).into_owned());

            let task_id = enqueue_task(prompt);

            let mut body = String::with_capacity(48);
            body.push_str("{\"queued\":true,\"task_id\":");
            body.push_str(&task_id.to_string());
            body.push('}');
            json_response(202, &body)
        }

        _ => json_response(404, "{\"error\":\"not found\"}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Canister lifecycle
// ═══════════════════════════════════════════════════════════════════════

fn restore_counters() {
    let msg_max = CHAT_LOG.with(|c| c.borrow().iter().last().map(|(k, _)| k).unwrap_or(0));
    MSG_COUNTER.with(|c| *c.borrow_mut() = msg_max);

    let task_max = TASK_QUEUE.with(|q| q.borrow().iter().last().map(|(k, _)| k).unwrap_or(0));
    TASK_COUNTER.with(|c| *c.borrow_mut() = task_max);
}

#[ic_cdk::init]
fn init() {
    restore_counters();
}

#[ic_cdk::post_upgrade]
fn post_upgrade() {
    restore_counters();
    // Migrate API key from legacy config field → dedicated secure cell
    CONFIG.with(|c| {
        let mut cell = c.borrow_mut();
        let mut cfg = cell.get().clone();
        if let Some(key) = cfg.api_key.take() {
            if key != "***" && !key.is_empty() {
                API_KEY_STORE.with(|k| { let _ = k.borrow_mut().set(SecretString(key.into_bytes())); });
            }
        }
        let defaults = AgentConfig::default();
        cfg.model = defaults.model;
        cfg.system_prompt = defaults.system_prompt;
        let _ = cell.set(cfg);
    });
}
