#!/usr/bin/env node
/**
 * ShellMind Vision — Universal AI Desktop Automation Engine
 * Hardened Localhost HTTP / Streamable-HTTP / SSE Daemon on Port 39200
 */
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { URL } from 'node:url';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { CulMcpBridge } from './lib/bridge.mjs';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const ROOT_DIR = process.env.SHELLMIND_VISION_ROOT || __dirname;
const PORT = Number(process.env.SHELLMIND_VISION_PORT || process.env.DESKTOP_UI_MCP_PORT) || 39200;
const HOST = process.env.SHELLMIND_VISION_HOST || process.env.DESKTOP_UI_MCP_HOST || '127.0.0.1';

let PKG_VERSION = '2.2.0';
try {
  const pkg = JSON.parse(fs.readFileSync(path.join(__dirname, 'package.json'), 'utf8'));
  if (typeof pkg.version === 'string' && pkg.version.trim()) PKG_VERSION = pkg.version.trim();
} catch {}

const TOKEN_DIR = path.join(ROOT_DIR, 'data');
const TOKEN_FILE = path.join(TOKEN_DIR, 'shellmind-vision.token');
fs.mkdirSync(TOKEN_DIR, { recursive: true });
let AUTH_TOKEN = '';
try {
  AUTH_TOKEN = fs.readFileSync(TOKEN_FILE, 'utf8').trim();
} catch {}
if (!AUTH_TOKEN || AUTH_TOKEN.length < 32) {
  AUTH_TOKEN = crypto.randomBytes(32).toString('hex');
  fs.writeFileSync(TOKEN_FILE, AUTH_TOKEN + '\n', { mode: 0o600 });
}
try { fs.chmodSync(TOKEN_FILE, 0o600); } catch {}

const LOCAL_HOSTS = new Set([
  `${HOST}:${PORT}`.toLowerCase(),
  `127.0.0.1:${PORT}`,
  `localhost:${PORT}`,
  `[::1]:${PORT}`,
]);

const ALLOWED_HOSTS = new Set(LOCAL_HOSTS);
for (const entry of String(process.env.SHELLMIND_VISION_ALLOWED_HOSTS || '').split(',')) {
  const h = entry.trim().toLowerCase();
  if (!h || h.includes('*') || h.includes('/') || h.includes('://') || /\s/.test(h)) continue;
  ALLOWED_HOSTS.add(h);
}

const MAX_SSE_SESSIONS = 64;
const SUPPORTED_PROTOCOLS = new Set(['2024-11-05', '2025-03-26', '2025-06-18', '2025-11-25']);

const bridge = new CulMcpBridge();
const sseSessions = new Map();
const mcpSessions = new Set();

function sendJson(res, status, data, extraHeaders = {}) {
  if (res.headersSent || res.destroyed) return;
  const body = Buffer.from(JSON.stringify(data));
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': body.length,
    ...extraHeaders,
  });
  res.end(body);
}

function reject(res, status, message) {
  return sendJson(res, status, { error: message });
}

function hasValidBearer(req) {
  const rawAuth = String(req.headers.authorization || '');
  const provided = rawAuth.startsWith('Bearer ') ? rawAuth.slice(7).trim() : '';
  const a = Buffer.from(provided);
  const b = Buffer.from(AUTH_TOKEN);
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}

function isDirectLocalHealthCaller(req) {
  const host = String(req.headers.host || '').toLowerCase();
  return (
    LOCAL_HOSTS.has(host) &&
    !req.headers['x-forwarded-for'] &&
    !req.headers['cf-connecting-ip']
  );
}

function guardRequest(req, res, requireAuth = true) {
  const host = String(req.headers.host || '').toLowerCase();
  if (!ALLOWED_HOSTS.has(host)) {
    reject(res, 403, 'Forbidden Host');
    return false;
  }
  if (req.headers.origin) {
    reject(res, 403, 'Browser Origin rejected');
    return false;
  }
  if (req.method === 'POST') {
    const ct = String(req.headers['content-type'] || '');
    if (!/^application\/json\b/i.test(ct)) {
      reject(res, 415, 'Content-Type must be application/json');
      return false;
    }
  }
  if (requireAuth && !hasValidBearer(req)) {
    reject(res, 401, 'Unauthorized');
    return false;
  }
  return true;
}

function readBody(req, limit = 16 * 1024 * 1024) {
  return new Promise((resolve, rejectErr) => {
    let total = 0;
    let done = false;
    const chunks = [];
    const onData = (c) => {
      if (done) return;
      total += c.length;
      if (total > limit) {
        done = true;
        req.removeListener('data', onData);
        const err = new Error('Request body too large');
        err.statusCode = 413;
        rejectErr(err);
        return;
      }
      chunks.push(c);
    };
    req.on('data', onData);
    req.on('end', () => {
      if (!done) {
        done = true;
        resolve(Buffer.concat(chunks).toString('utf8'));
      }
    });
    req.on('error', (err) => {
      if (!done) {
        done = true;
        rejectErr(err);
      }
    });
  });
}

setInterval(() => {
  for (const [sid, sseRes] of sseSessions.entries()) {
    if (sseRes.destroyed || sseRes.writableEnded) {
      sseSessions.delete(sid);
      continue;
    }
    try {
      sseRes.write(':\n\n');
    } catch {
      sseSessions.delete(sid);
    }
  }
}, 25000).unref();

const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url || '/', `http://${HOST}:${PORT}`);

    // Fast static liveness probe (never spawns upstream helper)
    if (req.method === 'GET' && (url.pathname === '/' || url.pathname === '/health' || url.pathname === '/status')) {
      if (!guardRequest(req, res, false)) return;
      if (isDirectLocalHealthCaller(req) || hasValidBearer(req)) {
        return sendJson(res, 200, {
          ok: true,
          service: 'ShellMind Vision',
          version: PKG_VERSION,
          pid: process.pid,
          root: ROOT_DIR,
          host: HOST,
          port: PORT,
        });
      }
      return sendJson(res, 200, {
        ok: true,
        service: 'ShellMind Vision',
        version: PKG_VERSION,
      });
    }

    if (!guardRequest(req, res, true)) return;

    // Legacy SSE endpoint (/sse)
    if (req.method === 'GET' && url.pathname === '/sse') {
      if (sseSessions.size >= MAX_SSE_SESSIONS) {
        return reject(res, 429, 'Too many active SSE sessions');
      }
      const sessionId = crypto.randomUUID();
      res.writeHead(200, {
        'Content-Type': 'text/event-stream',
        'Cache-Control': 'no-cache, no-transform',
        Connection: 'keep-alive',
      });
      res.write(`event: endpoint\ndata: /messages?sessionId=${sessionId}\n\n`);
      sseSessions.set(sessionId, res);
      res.on('error', () => sseSessions.delete(sessionId));
      req.on('close', () => sseSessions.delete(sessionId));
      return;
    }

    // Legacy SSE POST message endpoint (/messages)
    if (req.method === 'POST' && url.pathname === '/messages') {
      const sessionId = url.searchParams.get('sessionId');
      const sseRes = sessionId ? sseSessions.get(sessionId) : null;
      if (!sseRes || sseRes.destroyed || sseRes.writableEnded) {
        return reject(res, 404, 'Unknown or closed SSE sessionId');
      }
      const raw = await readBody(req);
      let msg;
      try {
        msg = JSON.parse(raw);
      } catch {
        return sendJson(res, 400, { error: 'Invalid JSON' });
      }
      const reply = await bridge.handleJsonRpc(msg);
      if (reply && !sseRes.writableEnded && !sseRes.destroyed) {
        sseRes.write(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
      }
      res.writeHead(202);
      return res.end('Accepted');
    }

    // Streamable HTTP MCP endpoint (/mcp)
    if (url.pathname === '/mcp') {
      if (req.method === 'POST') {
        const raw = await readBody(req);
        let msg;
        try {
          msg = JSON.parse(raw);
        } catch {
          return sendJson(res, 400, { error: 'Invalid JSON' });
        }
        const reply = await bridge.handleJsonRpc(msg);
        if (!reply) {
          res.writeHead(202);
          return res.end();
        }
        let sid = String(req.headers['mcp-session-id'] || '');
        if (msg.method === 'initialize' || !sid || !mcpSessions.has(sid)) {
          sid = crypto.randomUUID();
          mcpSessions.add(sid);
          if (mcpSessions.size > 256) {
            const oldest = mcpSessions.values().next().value;
            mcpSessions.delete(oldest);
          }
        }
        const reqProto = String(req.headers['mcp-protocol-version'] || '');
        const proto = SUPPORTED_PROTOCOLS.has(reqProto) ? reqProto : '2024-11-05';
        return sendJson(res, 200, reply, {
          'Mcp-Session-Id': sid,
          'Mcp-Protocol-Version': proto,
        });
      }

      if (req.method === 'GET') {
        return sendJson(
          res,
          405,
          { jsonrpc: '2.0', error: { code: -32000, message: 'Use POST /mcp for Streamable HTTP or GET /sse for SSE' } },
          { Allow: 'POST, DELETE' }
        );
      }

      if (req.method === 'DELETE') {
        const sid = String(req.headers['mcp-session-id'] || '');
        if (sid) mcpSessions.delete(sid);
        res.writeHead(200);
        return res.end();
      }
    }

    return sendJson(res, 404, { error: 'Not found', path: url.pathname });
  } catch (err) {
    const code = err?.statusCode || 500;
    sendJson(res, code, { error: err?.message || String(err) }, code === 413 ? { Connection: 'close' } : {});
    if (code === 413) {
      res.once('finish', () => req.destroy());
    }
    return;
  }
});

server.on('clientError', (_err, sock) => {
  if (!sock.destroyed) sock.destroy();
});

server.on('error', (err) => {
  if (err?.code === 'EADDRINUSE') {
    console.error(`[ShellMind Vision] Port ${PORT} already bound; exiting cleanly.`);
    process.exit(0);
  }
  console.error('[ShellMind Vision] Server error:', err);
  process.exit(1);
});

process.on('uncaughtException', (err) => {
  console.error('[ShellMind Vision] Uncaught exception:', err);
});

process.on('unhandledRejection', (err) => {
  console.error('[ShellMind Vision] Unhandled rejection:', err);
});

server.listen(PORT, HOST, () => {
  console.log(`[ShellMind Vision] Listening on http://${HOST}:${PORT}/mcp (and /sse, /health)`);
});
