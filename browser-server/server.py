"""
PicoClaw Browser Server — Scrapling + Chutes AI powered web intelligence.
Real stealth scraping + LLM synthesis. Bypasses anti-bot, gets actual data.
- /search: DuckDuckGo via StealthyFetcher + AI summary
- /browse: Stealth scrape any URL + AI extraction
- /ask: Search + scrape top results + AI answer with real data
- /hit: Hit a URL and return status/headers (no scraping or AI)
- /price: Live crypto prices (CoinGecko API)
- / : Dashboard UI (login required)
"""
import os
import re
import html
import time
import hashlib
import secrets
import asyncio
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import quote_plus
from xml.etree import ElementTree

import httpx
from fastapi import FastAPI, Query, Request, Cookie
from fastapi.responses import JSONResponse, HTMLResponse, RedirectResponse
from scrapling.fetchers import Fetcher, StealthyFetcher

app = FastAPI(title="PicoClaw Browser Server")

CHUTES_KEY = os.getenv(
    "CHUTES_API_KEY",
    "cpk_c3137eff6f414dabbdb4321ef4d76338.c664f41005b754f78d67821cdf12075d.5IMBvgCGG0BY7Nyd1xS2Dg3jaHt5kf9t",
)
CHUTES_URL = "https://llm.chutes.ai/v1/chat/completions"
MODEL = "deepseek-ai/DeepSeek-V3"
SYSTEM = (
    "You are PicoClaw's web intelligence engine. "
    "Rules: plain text ONLY (no markdown, no **, no #, no bullet symbols). "
    "Be concise — max 3-5 sentences per topic. "
    "Lead with the most important facts. "
    "Include dates and sources inline. "
    "If information is stale or unclear, say so."
)

# Canister API key
CANISTER_API_KEY = os.getenv("CANISTER_API_KEY", "pico_ca06ade4ccc876a78cb50b7091cd0189ad77984af38f9d8e627214f97a9ef10d")

# Auth
AUTH_USER = "markcryer"
AUTH_PASS_HASH = hashlib.sha256("Amazonkindle1".encode()).hexdigest()
_sessions: dict[str, float] = {}

# Activity log
_activity_log: list[dict] = []
MAX_LOG = 50

# Thread pool for sync scrapling calls
_executor = ThreadPoolExecutor(max_workers=4)


def log_activity(action: str, detail: str, status: str = "ok"):
    _activity_log.insert(0, {
        "time": time.strftime("%H:%M:%S"),
        "action": action,
        "detail": detail[:120],
        "status": status,
    })
    if len(_activity_log) > MAX_LOG:
        _activity_log.pop()


def check_session(token: str | None) -> bool:
    if not token or token not in _sessions:
        return False
    if time.time() > _sessions[token]:
        del _sessions[token]
        return False
    return True


# ── Scrapling Search (DuckDuckGo + StealthyFetcher) ──────────────

def _resolve_ddg_url(raw: str) -> str:
    """Extract real URL from DuckDuckGo redirect link."""
    if "uddg=" in raw:
        from urllib.parse import unquote, urlparse, parse_qs
        try:
            parsed = urlparse(raw)
            qs = parse_qs(parsed.query)
            if "uddg" in qs:
                return unquote(qs["uddg"][0])
        except Exception:
            pass
    if raw.startswith("http"):
        return raw
    return ""


def _ddg_search(query: str, num: int = 8) -> list[dict]:
    """Search DuckDuckGo HTML with stealth. Runs in thread (sync)."""
    url = f"https://html.duckduckgo.com/html/?q={quote_plus(query)}"
    page = StealthyFetcher.fetch(url)
    results = []
    for r in page.css(".result"):
        title = (r.css(".result__title a::text").get() or "").strip()
        snippet = (r.css(".result__snippet::text").get() or "").strip()
        raw_href = r.css(".result__title a::attr(href)").get() or ""
        href = _resolve_ddg_url(raw_href)
        source = (r.css(".result__url::text").get() or "").strip()
        if title and href:
            results.append({"title": title, "url": href, "snippet": snippet, "source": source})
        if len(results) >= num:
            break
    return results


def _scrape_url(url: str) -> str:
    """Scrape a URL with stealth and return text content. Runs in thread."""
    try:
        page = StealthyFetcher.fetch(url)
        # Get all text, clean it up
        texts = page.css("article::text, main::text, .content::text, p::text, h1::text, h2::text, h3::text, li::text, td::text").getall()
        if not texts:
            texts = page.css("body::text").getall()
        cleaned = [t.strip() for t in texts if t.strip() and len(t.strip()) > 5]
        return "\n".join(cleaned)[:6000]
    except Exception as e:
        return f"Error scraping {url}: {e}"


async def ddg_search(query: str, num: int = 8) -> list[dict]:
    loop = asyncio.get_event_loop()
    return await loop.run_in_executor(_executor, _ddg_search, query, num)


async def scrape_url(url: str) -> str:
    loop = asyncio.get_event_loop()
    return await loop.run_in_executor(_executor, _scrape_url, url)


# ── Google News RSS (real article URLs + headlines) ───────────────

async def google_news_rss(query: str, num: int = 8) -> list[dict]:
    """Fetch Google News RSS for real article links with headlines and dates."""
    url = f"https://news.google.com/rss/search?q={quote_plus(query)}&hl=en-US&gl=US&ceid=US:en"
    try:
        async with httpx.AsyncClient(timeout=15) as client:
            resp = await client.get(url)
            resp.raise_for_status()
            root = ElementTree.fromstring(resp.text)
            results = []
            for item in root.iter("item"):
                title = (item.findtext("title") or "").strip()
                link = (item.findtext("link") or "").strip()
                pub = (item.findtext("pubDate") or "").strip()
                source = (item.findtext("source") or "").strip()
                if title and link:
                    results.append({"title": html.unescape(title), "url": link, "date": pub, "source": source})
                if len(results) >= num:
                    break
            return results
    except Exception:
        return []


def _is_news_query(q: str) -> bool:
    """Detect if the query is news/current-events related."""
    q_lower = q.lower()
    news_words = ["news", "latest", "recent", "today", "breaking", "headlines",
                  "happened", "update", "current events", "this week", "this month",
                  "what's going on", "whats going on", "what is happening"]
    return any(w in q_lower for w in news_words)


# ── LLM via Chutes ────────────────────────────────────────────────

async def chutes_chat(prompt: str, system: str = SYSTEM, max_tokens: int = 512) -> str:
    messages = [{"role": "system", "content": system}]
    messages.append({"role": "user", "content": prompt})
    async with httpx.AsyncClient(timeout=45) as client:
        resp = await client.post(
            CHUTES_URL,
            headers={"Authorization": f"Bearer {CHUTES_KEY}", "Content-Type": "application/json"},
            json={"model": MODEL, "messages": messages, "temperature": 0.2, "max_tokens": max_tokens},
        )
        resp.raise_for_status()
        data = resp.json()
        return data["choices"][0]["message"]["content"]


# ── Crypto Prices (CoinGecko API) ────────────────────────────────

async def get_crypto_price(coin: str = "bitcoin") -> dict:
    async with httpx.AsyncClient(timeout=10) as client:
        resp = await client.get(
            "https://api.coingecko.com/api/v3/simple/price",
            params={"ids": coin, "vs_currencies": "usd", "include_24hr_change": "true"},
        )
        resp.raise_for_status()
        data = resp.json()
        if coin in data:
            return {"coin": coin, "price_usd": data[coin].get("usd"), "change_24h": data[coin].get("usd_24h_change")}
        return {"error": f"Coin '{coin}' not found"}


# ── API Endpoints ─────────────────────────────────────────────────

@app.get("/search")
async def search(q: str = Query(..., description="Search query"), raw: bool = Query(False)):
    """Search DuckDuckGo with stealth scraping + AI summary."""
    try:
        t0 = time.time()
        results = await ddg_search(q)
        if raw or not results:
            log_activity("search", f"{q} ({len(results)} raw)", "ok")
            return JSONResponse({"query": q, "results": results})

        formatted = "\n".join(f"- {r['title']}: {r['snippet']}" for r in results)
        summary = await chutes_chat(
            f"Summarize these search results for: {q}\n\n{formatted}\n\n"
            "Concise briefing of the top stories/results. Include sources.",
            max_tokens=512,
        )
        elapsed = round(time.time() - t0, 1)
        log_activity("search", f"{q} ({len(results)} results, {elapsed}s)", "ok")
        return JSONResponse({"query": q, "summary": summary, "result_count": len(results), "elapsed": elapsed})
    except Exception as e:
        log_activity("search", f"{q}: {e}", "error")
        return JSONResponse({"error": str(e)}, status_code=500)


@app.get("/browse")
async def browse(url: str = Query(..., description="URL to browse"), raw: bool = Query(False)):
    """Stealth-scrape a URL + AI content extraction."""
    try:
        t0 = time.time()
        text = await scrape_url(url)
        if raw or not text:
            log_activity("browse", f"{url} ({len(text)} chars, raw)", "ok")
            return JSONResponse({"url": url, "content": text, "length": len(text)})

        extracted = await chutes_chat(
            f"Extract the key information from this page. Skip navigation/ads.\n\nURL: {url}\n\nContent:\n{text}",
            max_tokens=768,
        )
        elapsed = round(time.time() - t0, 1)
        log_activity("browse", f"{url} ({len(text)} raw, {elapsed}s)", "ok")
        return JSONResponse({"url": url, "content": extracted, "raw_length": len(text), "elapsed": elapsed})
    except Exception as e:
        log_activity("browse", f"{url}: {e}", "error")
        return JSONResponse({"error": str(e)}, status_code=500)


@app.get("/ask")
async def ask(q: str = Query(..., description="Question to answer")):
    """Agentic pipeline: search → AI picks pages → scrape → AI aggregates answer."""
    try:
        t0 = time.time()

        # ── Step 0: Live crypto price if relevant ──
        q_lower = q.lower()
        price_info = ""
        if any(w in q_lower for w in ["price", "btc", "bitcoin", "eth", "ethereum", "sol", "solana", "crypto"]):
            coin = "bitcoin"
            if "eth" in q_lower or "ethereum" in q_lower: coin = "ethereum"
            elif "sol" in q_lower or "solana" in q_lower: coin = "solana"
            try:
                pd = await get_crypto_price(coin)
                if "price_usd" in pd:
                    price_info = f"LIVE DATA: {coin} price is ${pd['price_usd']:,.2f} USD (24h change: {pd.get('change_24h', 0):.2f}%)\n\n"
            except Exception:
                pass

        # ── Step 1: Search (DDG + Google News in parallel) ──
        ddg_task = ddg_search(q, num=10)
        news_task = google_news_rss(q, num=8)
        ddg_results, news_results = await asyncio.gather(ddg_task, news_task)

        # Merge and deduplicate by domain
        all_results = []
        seen_domains = set()
        for r in ddg_results + news_results:
            url = r.get("url", "")
            if not url or not url.startswith("http"):
                continue
            domain = url.split("/")[2] if len(url.split("/")) > 2 else url
            if domain not in seen_domains:
                seen_domains.add(domain)
                all_results.append(r)

        if not all_results:
            answer = price_info + f"No search results found for: {q}" if price_info else f"No search results found for: {q}"
            log_activity("ask", f"{q}: no results", "warn")
            return JSONResponse({"answer": answer, "sources": []})

        # ── Step 2: AI picks which pages to scrape ──
        listing = "\n".join(
            f"[{i}] {r.get('title','')} | {r['url']}" + (f" | {r.get('snippet','')}" if r.get('snippet') else "") + (f" | {r.get('date','')}" if r.get('date') else "")
            for i, r in enumerate(all_results)
        )

        pick_prompt = (
            f"You are a research assistant. The user asked: \"{q}\"\n\n"
            f"Here are search results:\n{listing}\n\n"
            f"Pick the 3-5 BEST URLs to scrape for answering the question. "
            f"Choose pages most likely to contain actual data, facts, and details (not homepages or paywalled sites). "
            f"Reply with ONLY the numbers, comma-separated. Example: 0,2,4,7\n"
            f"Numbers only, nothing else."
        )
        pick_response = await chutes_chat(pick_prompt, system="You select URLs. Reply with comma-separated numbers only.", max_tokens=64)

        # Parse picked indices
        picked_indices = []
        for tok in re.findall(r"\d+", pick_response):
            idx = int(tok)
            if 0 <= idx < len(all_results):
                picked_indices.append(idx)
        # Fallback: if AI returned garbage, take first 4
        if not picked_indices:
            picked_indices = list(range(min(4, len(all_results))))
        # Cap at 5
        picked_indices = picked_indices[:5]

        scrape_urls = [all_results[i]["url"] for i in picked_indices]
        log_activity("ask", f"AI picked {len(scrape_urls)} pages to scrape", "ok")

        # ── Step 3: Scrape the AI-selected pages in parallel ──
        scrape_tasks = [scrape_url(u) for u in scrape_urls]
        pages = await asyncio.gather(*scrape_tasks, return_exceptions=True)

        scraped_content = ""
        for i, page_text in enumerate(pages):
            if isinstance(page_text, str) and not page_text.startswith("Error") and len(page_text) > 50:
                scraped_content += f"\n--- [{all_results[picked_indices[i]].get('title','')}] {scrape_urls[i]} ---\n{page_text[:3000]}\n"

        # ── Step 4: AI aggregates final answer ──
        search_summary = "\n".join(f"- {r.get('title','')}: {r.get('snippet','')}" for r in all_results[:10])
        context = f"{price_info}Search results summary:\n{search_summary}"
        if scraped_content:
            context += f"\n\nScraped page content:{scraped_content}"

        answer = await chutes_chat(
            f"Answer this question using ALL the data below. Be thorough and factual. "
            f"Include specific facts, numbers, names, dates from the scraped content. "
            f"Cite which source each fact came from. "
            f"If this is a news query, summarize each major story with key details.\n\n"
            f"Question: {q}\n\n{context}",
            max_tokens=1024,
        )

        elapsed = round(time.time() - t0, 1)
        sources = scrape_urls
        log_activity("ask", f"{q} ({elapsed}s, {len(all_results)} found, {len(scrape_urls)} scraped)", "ok")
        return JSONResponse({"answer": answer, "sources": sources, "scraped_count": len(scrape_urls), "elapsed": elapsed})
    except Exception as e:
        log_activity("ask", f"{q}: {e}", "error")
        return JSONResponse({"error": str(e)}, status_code=500)


@app.get("/price")
async def price(coin: str = Query("bitcoin")):
    """Live crypto price from CoinGecko API."""
    try:
        data = await get_crypto_price(coin)
        log_activity("price", f"{coin}: ${data.get('price_usd', '?')}", "ok")
        return JSONResponse(data)
    except Exception as e:
        log_activity("price", f"{coin}: {e}", "error")
        return JSONResponse({"error": str(e)}, status_code=500)


# ── Canister API: /api/intel ──────────────────────────────────────

FACT_SYSTEM = (
    "You extract ONLY meaningful facts from web data. Dense fact list only. "
    "Rules: lead with numbers, dates, names, prices. "
    "Cite source domain inline like: BTC $97K (coindesk). "
    "Use | to separate facts. No filler, no analysis, no complete sentences. "
    "No markdown. SKIP all navigation text, cookie notices, consent pages, "
    "privacy settings, 'about' sections, loading indicators, affiliate disclaimers, "
    "and any text that is website UI rather than actual content. "
    "Only include facts that directly answer the query."
)


@app.post("/api/intel")
async def api_intel(request: Request):
    """Web intelligence endpoint for ICP canister. Returns compressed facts."""
    # Auth
    if request.headers.get("X-Api-Key") != CANISTER_API_KEY:
        return JSONResponse({"ok": False, "e": "unauthorized"}, status_code=401)

    try:
        body = await request.json()
    except Exception:
        return JSONResponse({"ok": False, "e": "invalid json"}, status_code=400)

    query = body.get("query", "").strip()
    mode = body.get("mode", "search")
    url = body.get("url", "")
    max_bytes = min(body.get("max_bytes", 4000), 6000)

    if not query and mode != "price":
        return JSONResponse({"ok": False, "e": "missing query"}, status_code=400)

    try:
        t0 = time.time()

        if mode == "price":
            # Direct price lookup — no scraping needed
            coin = query or "bitcoin"
            q_lower = coin.lower()
            if "eth" in q_lower: coin = "ethereum"
            elif "sol" in q_lower: coin = "solana"
            elif "btc" in q_lower or "bit" in q_lower: coin = "bitcoin"
            pd = await get_crypto_price(coin)
            if "price_usd" in pd:
                facts = f"{coin} ${pd['price_usd']:,.2f} ({pd.get('change_24h', 0):+.2f}% 24h) (coingecko)"
            else:
                facts = f"Price not found for: {coin}"
            log_activity("intel", f"price:{coin}", "ok")
            return JSONResponse({"ok": True, "f": facts, "s": ["coingecko.com"], "t": int(time.time())})

        if mode == "browse":
            # Scrape single URL + compress
            if not url:
                return JSONResponse({"ok": False, "e": "missing url"}, status_code=400)
            text = await scrape_url(url)
            if not text or text.startswith("Error"):
                return JSONResponse({"ok": False, "e": f"scrape failed: {text[:200]}"})
            facts = await chutes_chat(
                f"Extract key facts from this page ({url}):\n\n{text[:5000]}\n\nBudget: {max_bytes} chars.",
                system=FACT_SYSTEM, max_tokens=512,
            )
            facts = _truncate_utf8(facts, max_bytes)
            domain = url.split("/")[2] if len(url.split("/")) > 2 else url
            elapsed = round(time.time() - t0, 1)
            log_activity("intel", f"browse:{url} ({elapsed}s)", "ok")
            return JSONResponse({"ok": True, "f": facts, "s": [domain], "t": int(time.time())})

        # mode == "search" (default)
        # Step 0: If query is about crypto price, prepend live price
        price_prefix = ""
        q_lower = query.lower()
        if any(w in q_lower for w in ["price", "btc", "bitcoin", "eth", "ethereum", "sol", "solana"]):
            coin = "bitcoin"
            if "eth" in q_lower or "ethereum" in q_lower: coin = "ethereum"
            elif "sol" in q_lower or "solana" in q_lower: coin = "solana"
            try:
                pd = await get_crypto_price(coin)
                if "price_usd" in pd:
                    price_prefix = f"{coin} ${pd['price_usd']:,.2f} ({pd.get('change_24h', 0):+.2f}% 24h) (coingecko) | "
            except Exception:
                pass

        # Step 1: Search DDG + Google News in parallel
        ddg_task = ddg_search(query, num=10)
        news_task = google_news_rss(query, num=8)
        ddg_results, news_results = await asyncio.gather(ddg_task, news_task)

        # Merge and deduplicate
        all_results = []
        seen_domains = set()
        for r in ddg_results + news_results:
            r_url = r.get("url", "")
            if not r_url or not r_url.startswith("http"):
                continue
            domain = r_url.split("/")[2] if len(r_url.split("/")) > 2 else r_url
            if domain not in seen_domains:
                seen_domains.add(domain)
                all_results.append(r)

        if not all_results:
            return JSONResponse({"ok": True, "f": f"No results found for: {query}", "s": [], "t": int(time.time())})

        # Step 2: AI picks best pages to scrape
        listing = "\n".join(
            f"[{i}] {r.get('title','')} | {r['url']}"
            for i, r in enumerate(all_results)
        )
        pick_response = await chutes_chat(
            f"User asked: \"{query}\"\n\nSearch results:\n{listing}\n\n"
            f"Pick 3-5 best URLs for real data (not homepages/paywalls). Reply numbers only, comma-separated.",
            system="You select URLs. Reply with comma-separated numbers only.",
            max_tokens=64,
        )
        picked = []
        for tok in re.findall(r"\d+", pick_response):
            idx = int(tok)
            if 0 <= idx < len(all_results):
                picked.append(idx)
        if not picked:
            picked = list(range(min(4, len(all_results))))
        picked = picked[:5]

        scrape_urls = [all_results[i]["url"] for i in picked]

        # Step 3: Scrape in parallel
        pages = await asyncio.gather(*[scrape_url(u) for u in scrape_urls], return_exceptions=True)
        scraped = ""
        for i, text in enumerate(pages):
            if isinstance(text, str) and not text.startswith("Error") and len(text) > 50:
                scraped += f"\n[{all_results[picked[i]].get('title','')}] ({scrape_urls[i]})\n{text[:2500]}\n"

        # Step 4: Compress facts
        search_listing = "\n".join(f"- {r.get('title','')}: {r.get('snippet','')}" for r in all_results[:10])
        compress_input = f"Question: {query}\n\nSearch headlines:\n{search_listing}"
        if scraped:
            compress_input += f"\n\nScraped pages:{scraped}"

        facts = await chutes_chat(
            f"{compress_input}\n\nExtract ONLY facts that answer the question. "
            f"Skip website UI, navigation, cookie/consent text, loading messages, disclaimers. "
            f"Budget: {max_bytes} chars.",
            system=FACT_SYSTEM, max_tokens=768,
        )
        facts = price_prefix + facts
        facts = _truncate_utf8(facts, max_bytes)

        sources = [scrape_urls[i].split("/")[2] if len(scrape_urls[i].split("/")) > 2 else scrape_urls[i] for i in range(len(scrape_urls))]
        if price_prefix:
            sources.insert(0, "coingecko.com")
        elapsed = round(time.time() - t0, 1)
        log_activity("intel", f"search:{query} ({elapsed}s, {len(scrape_urls)} scraped)", "ok")
        return JSONResponse({"ok": True, "f": facts, "s": sources, "t": int(time.time())})

    except Exception as e:
        log_activity("intel", f"{mode}:{query}: {e}", "error")
        return JSONResponse({"ok": False, "e": str(e)[:200]}, status_code=500)


def _truncate_utf8(text: str, max_bytes: int) -> str:
    """Truncate text to fit within max_bytes when UTF-8 encoded."""
    encoded = text.encode("utf-8")
    if len(encoded) <= max_bytes:
        return text
    truncated = encoded[:max_bytes]
    return truncated.decode("utf-8", errors="ignore")


@app.get("/hit")
async def hit(url: str = Query(..., description="URL to hit")):
    """Hit a URL and return status code, headers, and response time. No scraping or AI."""
    try:
        t0 = time.time()
        async with httpx.AsyncClient(timeout=15, follow_redirects=True) as client:
            resp = await client.get(url)
        elapsed = round(time.time() - t0, 3)
        headers = dict(resp.headers)
        log_activity("hit", f"{url} → {resp.status_code} ({elapsed}s)", "ok")
        return JSONResponse({
            "url": url,
            "status_code": resp.status_code,
            "content_type": headers.get("content-type", ""),
            "content_length": int(headers.get("content-length", 0)) or len(resp.content),
            "elapsed": elapsed,
            "headers": headers,
        })
    except Exception as e:
        log_activity("hit", f"{url}: {e}", "error")
        return JSONResponse({"url": url, "error": str(e)}, status_code=502)


@app.get("/health")
async def health():
    return {"status": "ok", "service": "picoclaw-browser", "model": MODEL, "engine": "scrapling"}


@app.get("/api/log")
async def api_log(session: str | None = Cookie(None)):
    if not check_session(session):
        return JSONResponse({"error": "unauthorized"}, status_code=401)
    return JSONResponse({"log": _activity_log})


# ── Auth + Dashboard ──────────────────────────────────────────────

@app.post("/login")
async def login(request: Request):
    form = await request.form()
    user = form.get("username", "")
    pw = form.get("password", "")
    if user == AUTH_USER and hashlib.sha256(pw.encode()).hexdigest() == AUTH_PASS_HASH:
        token = secrets.token_hex(24)
        _sessions[token] = time.time() + 86400
        resp = RedirectResponse("/", status_code=303)
        resp.set_cookie("session", token, httponly=True, max_age=86400)
        log_activity("auth", f"login: {user}", "ok")
        return resp
    log_activity("auth", f"failed login: {user}", "error")
    return HTMLResponse(LOGIN_PAGE.replace("<!--ERR-->", '<p class="err">Invalid credentials</p>'), status_code=401)


@app.get("/logout")
async def logout(session: str | None = Cookie(None)):
    if session and session in _sessions:
        del _sessions[session]
    resp = RedirectResponse("/", status_code=303)
    resp.delete_cookie("session")
    return resp


@app.get("/")
async def dashboard(session: str | None = Cookie(None)):
    if not check_session(session):
        return HTMLResponse(LOGIN_PAGE)
    return HTMLResponse(DASHBOARD_PAGE)


# ── HTML ──────────────────────────────────────────────────────────

LOGIN_PAGE = """<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>PicoClaw Browser - Login</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;background:#0a0a0f;color:#e0e0e0;min-height:100vh;display:flex;align-items:center;justify-content:center}
.login-box{background:#14141f;border:1px solid #2a2a3a;border-radius:16px;padding:40px;width:380px;box-shadow:0 20px 60px rgba(0,0,0,0.5)}
.logo{text-align:center;margin-bottom:32px}
.logo h1{font-size:24px;color:#8b5cf6;font-weight:700}
.logo p{color:#666;font-size:13px;margin-top:4px}
label{display:block;font-size:12px;color:#888;text-transform:uppercase;letter-spacing:1px;margin-bottom:6px;margin-top:16px}
input[type=text],input[type=password]{width:100%;padding:12px 14px;background:#0a0a0f;border:1px solid #2a2a3a;border-radius:8px;color:#fff;font-size:15px;outline:none;transition:border .2s}
input:focus{border-color:#8b5cf6}
button{width:100%;padding:12px;background:#8b5cf6;color:#fff;border:none;border-radius:8px;font-size:15px;font-weight:600;cursor:pointer;margin-top:24px;transition:background .2s}
button:hover{background:#7c3aed}
.err{color:#ef4444;font-size:13px;text-align:center;margin-top:12px}
.dot{display:inline-block;width:8px;height:8px;background:#22c55e;border-radius:50%;margin-right:6px;vertical-align:middle}
</style></head><body>
<div class="login-box">
  <div class="logo">
    <h1><span class="dot"></span>PicoClaw Browser</h1>
    <p>AI-Powered Web Intelligence</p>
  </div>
  <form method="POST" action="/login">
    <label>Username</label>
    <input type="text" name="username" autofocus required>
    <label>Password</label>
    <input type="password" name="password" required>
    <button type="submit">Sign In</button>
  </form>
  <!--ERR-->
</div></body></html>"""

DASHBOARD_PAGE = """<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>PicoClaw Browser Server</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;background:#0a0a0f;color:#e0e0e0;min-height:100vh}
header{background:#14141f;border-bottom:1px solid #1e1e2e;padding:12px 24px;display:flex;align-items:center;justify-content:space-between}
header h1{font-size:18px;color:#8b5cf6;font-weight:700}
header .right{display:flex;align-items:center;gap:16px}
.status{font-size:12px;color:#22c55e;display:flex;align-items:center;gap:6px}
.status .dot{width:7px;height:7px;background:#22c55e;border-radius:50%;display:inline-block}
.tag{font-size:11px;background:#1e1e2e;padding:4px 10px;border-radius:12px}
.tag-model{color:#8b5cf6}
.tag-engine{color:#f59e0b}
a.logout{color:#666;font-size:12px;text-decoration:none}
a.logout:hover{color:#ef4444}
.container{max-width:1100px;margin:0 auto;padding:24px}
.price-row{display:flex;gap:10px;margin-bottom:20px;flex-wrap:wrap}
.price-card{background:#14141f;border:1px solid #1e1e2e;border-radius:10px;padding:14px 20px;min-width:180px}
.price-card .coin{font-size:13px;color:#888;text-transform:uppercase}
.price-card .val{font-size:22px;font-weight:700;color:#fff;margin-top:4px}
.price-card .chg{font-size:12px;margin-top:2px}
.price-card .chg.up{color:#22c55e}
.price-card .chg.down{color:#ef4444}
.tabs{display:flex;gap:2px;margin-bottom:20px;background:#14141f;border-radius:10px;padding:4px;width:fit-content}
.tab{padding:8px 20px;border-radius:8px;cursor:pointer;font-size:13px;font-weight:500;color:#888;transition:all .2s;border:none;background:none}
.tab.active{background:#8b5cf6;color:#fff}
.tab:hover:not(.active){color:#ccc}
.panel{display:none}
.panel.active{display:block}
.input-row{display:flex;gap:10px;margin-bottom:20px}
.input-row input{flex:1;padding:12px 16px;background:#14141f;border:1px solid #2a2a3a;border-radius:10px;color:#fff;font-size:14px;outline:none}
.input-row input:focus{border-color:#8b5cf6}
.input-row button{padding:12px 28px;background:#8b5cf6;color:#fff;border:none;border-radius:10px;font-size:14px;font-weight:600;cursor:pointer;white-space:nowrap;transition:all .2s}
.input-row button:hover{background:#7c3aed}
.input-row button:disabled{opacity:0.5;cursor:not-allowed}
.result-box{background:#14141f;border:1px solid #1e1e2e;border-radius:12px;padding:20px;min-height:120px;white-space:pre-wrap;font-size:14px;line-height:1.7;color:#d0d0d0}
.result-box .label{font-size:11px;text-transform:uppercase;letter-spacing:1px;color:#666;margin-bottom:10px}
.result-box .meta{font-size:12px;color:#555;margin-top:12px;padding-top:12px;border-top:1px solid #1e1e2e}
.spinner{display:inline-block;width:16px;height:16px;border:2px solid #333;border-top-color:#8b5cf6;border-radius:50%;animation:spin .6s linear infinite;vertical-align:middle;margin-right:8px}
@keyframes spin{to{transform:rotate(360deg)}}
.steps{margin-top:6px;font-size:12px;color:#444}
.log-section{margin-top:32px}
.log-section h3{font-size:13px;color:#666;text-transform:uppercase;letter-spacing:1px;margin-bottom:12px}
.log-table{width:100%;border-collapse:collapse;font-size:12px}
.log-table th{text-align:left;padding:8px 12px;color:#555;border-bottom:1px solid #1e1e2e;font-weight:500}
.log-table td{padding:8px 12px;border-bottom:1px solid #0e0e18;color:#999}
.log-table .act{font-weight:600}
.log-table .act-search{color:#3b82f6}
.log-table .act-browse{color:#f59e0b}
.log-table .act-ask{color:#22c55e}
.log-table .act-price{color:#06b6d4}
.log-table .act-auth{color:#8b5cf6}
.log-table .st-ok{color:#22c55e}
.log-table .st-error{color:#ef4444}
.sources{margin-top:10px;font-size:12px;color:#555}
.sources a{color:#8b5cf6;text-decoration:none;word-break:break-all}
.sources a:hover{text-decoration:underline}
</style></head><body>
<header>
  <h1>PicoClaw Browser Server</h1>
  <div class="right">
    <span class="tag tag-engine">Scrapling</span>
    <span class="tag tag-model">DeepSeek-V3</span>
    <span class="status"><span class="dot"></span> Online</span>
    <a class="logout" href="/logout">Logout</a>
  </div>
</header>
<div class="container">
  <div class="price-row" id="prices"></div>
  <div class="tabs">
    <button class="tab active" onclick="switchTab('ask')">Ask</button>
    <button class="tab" onclick="switchTab('search')">Search</button>
    <button class="tab" onclick="switchTab('browse')">Browse</button>
  </div>
  <div id="panel-ask" class="panel active">
    <div class="input-row">
      <input id="ask-input" type="text" placeholder="Ask anything... e.g. What is the current BTC price?" onkeydown="if(event.key==='Enter')doAction('ask')">
      <button id="ask-btn" onclick="doAction('ask')">Ask</button>
    </div>
    <div class="result-box">
      <div class="label">AI Answer</div>
      <div id="ask-content" style="color:#555">Ask a question. The AI will search DuckDuckGo, scrape the top results, and answer with real data.</div>
      <div id="ask-sources" class="sources"></div>
      <div id="ask-meta" class="meta"></div>
    </div>
  </div>
  <div id="panel-search" class="panel">
    <div class="input-row">
      <input id="search-input" type="text" placeholder="Search query..." onkeydown="if(event.key==='Enter')doAction('search')">
      <button id="search-btn" onclick="doAction('search')">Search</button>
    </div>
    <div class="result-box">
      <div class="label">AI Summary</div>
      <div id="search-content" style="color:#555">Enter a query to search with stealth scraping.</div>
      <div id="search-meta" class="meta"></div>
    </div>
  </div>
  <div id="panel-browse" class="panel">
    <div class="input-row">
      <input id="browse-input" type="text" placeholder="URL to browse..." onkeydown="if(event.key==='Enter')doAction('browse')">
      <button id="browse-btn" onclick="doAction('browse')">Browse</button>
    </div>
    <div class="result-box">
      <div class="label">Extracted Content</div>
      <div id="browse-content" style="color:#555">Enter a URL to scrape and extract content.</div>
      <div id="browse-meta" class="meta"></div>
    </div>
  </div>
  <div class="log-section">
    <h3>Activity Log</h3>
    <table class="log-table">
      <thead><tr><th>Time</th><th>Action</th><th>Detail</th><th>Status</th></tr></thead>
      <tbody id="log-body"><tr><td colspan="4" style="color:#444">No activity yet</td></tr></tbody>
    </table>
  </div>
</div>
<script>
function switchTab(name){document.querySelectorAll('.tab').forEach(t=>t.classList.remove('active'));document.querySelectorAll('.panel').forEach(p=>p.classList.remove('active'));event.target.classList.add('active');document.getElementById('panel-'+name).classList.add('active');document.querySelector('#panel-'+name+' input')?.focus()}
function setLoading(id,on){const b=document.getElementById(id+'-btn'),c=document.getElementById(id+'-content');if(on){b.disabled=true;c.innerHTML='<span class="spinner"></span> AI is working...<div class="steps">Searching DDG + Google News → AI picks best pages → Scraping → AI aggregates answer</div>';c.style.color='#888'}else b.disabled=false}
async function doAction(type){const input=document.getElementById(type+'-input').value.trim();if(!input)return;setLoading(type,true);const t0=Date.now();try{const param=type==='browse'?'url':'q';const r=await fetch('/'+type+'?'+param+'='+encodeURIComponent(input));const d=await r.json();const el=((Date.now()-t0)/1000).toFixed(1);const text=d.answer||d.summary||d.content||d.error||'No result';document.getElementById(type+'-content').textContent=text;document.getElementById(type+'-content').style.color=d.error?'#ef4444':'#d0d0d0';document.getElementById(type+'-meta').textContent=el+'s'+(d.elapsed?' (server: '+d.elapsed+'s)':'')+(d.result_count?' | '+d.result_count+' sources':'')+(d.scraped_count?' | '+d.scraped_count+' pages scraped':'');const srcEl=document.getElementById(type+'-sources');if(srcEl&&d.sources&&d.sources.length){srcEl.innerHTML=d.sources.slice(0,5).map(s=>'<a href="'+s+'" target="_blank">'+s.substring(0,70)+'...</a>').join('<br>')}else if(srcEl)srcEl.innerHTML=''}catch(e){document.getElementById(type+'-content').textContent='Error: '+e.message;document.getElementById(type+'-content').style.color='#ef4444'}setLoading(type,false);refreshLog()}
async function loadPrices(){const coins=['bitcoin','ethereum','solana'];const c=document.getElementById('prices');c.innerHTML='';for(const coin of coins){try{const r=await fetch('/price?coin='+coin);const d=await r.json();if(d.price_usd){const chg=d.change_24h||0;const cls=chg>=0?'up':'down';const sign=chg>=0?'+':'';c.innerHTML+='<div class="price-card"><div class="coin">'+coin+'</div><div class="val">$'+Number(d.price_usd).toLocaleString(undefined,{minimumFractionDigits:2,maximumFractionDigits:2})+'</div><div class="chg '+cls+'">'+sign+chg.toFixed(2)+'%</div></div>'}}catch(e){}}}
async function refreshLog(){try{const r=await fetch('/api/log');const d=await r.json();if(!d.log||!d.log.length)return;document.getElementById('log-body').innerHTML=d.log.map(e=>'<tr><td>'+e.time+'</td><td class="act act-'+e.action+'">'+e.action+'</td><td>'+e.detail+'</td><td class="st-'+e.status+'">'+e.status+'</td></tr>').join('')}catch(e){}}
loadPrices();setInterval(loadPrices,60000);setInterval(refreshLog,5000);refreshLog()
</script>
</body></html>"""

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8042)
