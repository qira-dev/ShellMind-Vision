#!/usr/bin/env node
/**
 * ShellMind Vision Stdio Transport Entrypoint (Singleton Daemon Proxy)
 * Routes stdio MCP requests through the authenticated singleton HTTP daemon
 * on http://127.0.0.1:39200/mcp with zero duplicate action execution.
 */
import { execFile } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CulMcpBridge } from './lib/bridge.mjs';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const ROOT_DIR = process.env.SHELLMIND_VISION_ROOT || __dirname;
const PORT = Number(process.env.SHELLMIND_VISION_PORT || process.env.DESKTOP_UI_MCP_PORT) || 39200;
const HOST = process.env.SHELLMIND_VISION_HOST || process.env.DESKTOP_UI_MCP_HOST || '127.0.0.1';
const DAEMON_URL = `http://${HOST}:${PORT}/mcp`;
const HEALTH_URL = `http://${HOST}:${PORT}/health`;
const TOKEN_DIR = path.join(ROOT_DIR, 'data');
const TOKEN_FILE = path.join(TOKEN_DIR, 'shellmind-vision.token');

let fallbackBridge = null;
let ensureDaemonPromise = null;

function readAuthToken() {
  try {
    return fs.readFileSync(TOKEN_FILE, 'utf8').trim();
  } catch {
    return '';
  }
}

async function isDaemonUp() {
  try {
    const res = await fetch(HEALTH_URL, { signal: AbortSignal.timeout(1500) });
    return res.ok;
  } catch {
    return false;
  }
}

async function ensureDaemonStarted() {
  if (await isDaemonUp()) return true;
  if (ensureDaemonPromise) return ensureDaemonPromise;

  ensureDaemonPromise = new Promise((resolve) => {
    execFile(path.join(ROOT_DIR, 'shellmind-vision'), ['up'], { timeout: 7000 }, async () => {
      const ok = await isDaemonUp();
      ensureDaemonPromise = null;
      resolve(ok);
    });
  });
  return ensureDaemonPromise;
}

async function forwardToDaemon(msg) {
  const up = await ensureDaemonStarted();
  if (up) {
    const token = readAuthToken();
    const headers = { 'Content-Type': 'application/json' };
    if (token) headers.Authorization = `Bearer ${token}`;

    try {
      const res = await fetch(DAEMON_URL, {
        method: 'POST',
        headers,
        body: JSON.stringify(msg),
        signal: AbortSignal.timeout(120000),
      });
      if (res.status === 202) return null;
      if (res.ok) {
        return await res.json();
      }
      const errText = await res.text().catch(() => `HTTP ${res.status}`);
      if (msg.id !== undefined) {
        return {
          jsonrpc: '2.0',
          id: msg.id,
          error: { code: -32603, message: `Daemon HTTP ${res.status}: ${errText.slice(0, 200)}` },
        };
      }
      return null;
    } catch (err) {
      // Only fall back to in-process bridge when connection was refused (never re-execute on mid-flight error)
      if (err?.cause?.code !== 'ECONNREFUSED') {
        if (msg.id !== undefined) {
          return {
            jsonrpc: '2.0',
            id: msg.id,
            error: { code: -32603, message: err?.message || String(err) },
          };
        }
        return null;
      }
    }
  }

  if (!fallbackBridge) {
    fallbackBridge = new CulMcpBridge();
  }
  return fallbackBridge.handleJsonRpc(msg);
}

let writeQueue = Promise.resolve();
function writeStdoutLine(line) {
  writeQueue = writeQueue.then(
    () =>
      new Promise((resolve) => {
        if (process.stdout.write(line + '\n')) {
          resolve();
        } else {
          process.stdout.once('drain', resolve);
        }
      })
  );
  return writeQueue;
}

let stdinBuf = '';
const inFlight = new Set();

process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  stdinBuf += chunk;
  let idx;
  while ((idx = stdinBuf.indexOf('\n')) !== -1) {
    const line = stdinBuf.slice(0, idx).trim();
    stdinBuf = stdinBuf.slice(idx + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    const p = (async () => {
      try {
        const reply = await forwardToDaemon(msg);
        if (reply) {
          await writeStdoutLine(JSON.stringify(reply));
        }
      } catch (err) {
        if (msg?.id !== undefined) {
          await writeStdoutLine(
            JSON.stringify({
              jsonrpc: '2.0',
              id: msg.id,
              error: { code: -32603, message: err?.message || String(err) },
            })
          );
        }
      }
    })();
    inFlight.add(p);
    p.finally(() => inFlight.delete(p));
  }
});

process.stdin.on('end', async () => {
  await Promise.allSettled([...inFlight]);
  await writeQueue;
  process.stdout.end(() => process.exit(0));
});
