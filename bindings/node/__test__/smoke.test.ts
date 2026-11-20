// Loads the built addon (`bun run build:debug` first) and runs it against a Bun.serve fake of an
// HTTP origin. The fake answers on the JS thread while sync and pin run on the thread pool: a
// binding that blocked the event loop would hang here.

import { afterAll, beforeAll, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { parseLock, pin, renderLock, sync, verify } from "../index.js";

const sha = (bytes: Uint8Array | string) => createHash("sha256").update(bytes).digest("hex");

const blobs = new Map<string, Uint8Array>();
const corruptOnce = new Set<string>();
const gets: string[] = [];
let server: ReturnType<typeof Bun.serve>;
let dir: string;

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "assetsync-smoke-"));
  server = Bun.serve({
    port: 0,
    async fetch(req) {
      const name = new URL(req.url).pathname.replace(/^\/blobs\//, "");
      if (req.headers.get("authorization") !== "Bearer smoke-token") {
        return new Response("who are you", { status: 401 });
      }
      if (req.method === "GET" && name === "known") {
        return new Response([...blobs.keys()].join("\n"));
      }
      if (req.method === "PUT") {
        const body = new Uint8Array(await req.arrayBuffer());
        if (sha(body) !== name) return new Response("wrong bytes", { status: 400 });
        blobs.set(name, body);
        return new Response("");
      }
      const bytes = blobs.get(name);
      if (!bytes) return new Response("none", { status: 404 });
      gets.push(name);
      if (corruptOnce.delete(name)) {
        const bad = bytes.slice();
        bad[0] ^= 0xff;
        return new Response(bad);
      }
      return new Response(bytes);
    },
  });
});

afterAll(() => {
  server.stop(true);
  rmSync(dir, { recursive: true, force: true });
});

const origin = () => ({
  http: { baseUrl: `http://127.0.0.1:${server.port}/blobs`, token: "smoke-token" },
});
const retry = { attempts: 3, baseDelayMs: 2, maxDelayMs: 10 };

test("parseLock reads the lock format and renderLock writes it back", () => {
  const a = sha("a");
  const text = `# source smoke\n${a} 1 voice/a b.mp3\n${a} 1 voice/c.mp3\n`;
  const lock = parseLock(text);
  expect(lock.header).toEqual([{ key: "source", value: "smoke" }]);
  expect(lock.entries.map((e) => e.path)).toEqual(["voice/a b.mp3", "voice/c.mp3"]);
  expect(lock.entries[0].size).toBe(1);
  expect(renderLock({ header: lock.header, entries: [...lock.entries].reverse() })).toBe(text);
  expect(() => parseLock(`${a} 1 z\n${a} 1 a\n`)).toThrow(/sorted/);
  expect(() => parseLock(`${a} 1 ../escape\n`)).toThrow(/inside the tree/);
});

test("pin uploads a tree, and sync fetches it into a fresh cache, verified", async () => {
  const tree = join(dir, "made");
  mkdirSync(join(tree, "words"), { recursive: true });
  writeFileSync(join(tree, "words/cat.mp3"), "cat clip");
  writeFileSync(join(tree, "words/dog.mp3"), "dog clip");
  const lock = join(dir, "voice.lock");

  const pinned = await pin(lock, {
    root: tree,
    header: [{ key: "source", value: "smoke" }],
    origin: origin(),
    cacheDir: join(dir, "maker-cache"),
    quiet: true,
    retry,
  });
  expect(pinned).toMatchObject({ files: 2, blobs: 2, uploaded: 2, unavailable: [] });
  expect(blobs.size).toBe(2);
  expect(readFileSync(lock, "utf8")).toStartWith("# source smoke\n");

  // The first download of the cat clip arrives corrupt: it is fetched again, never kept.
  const cat = sha("cat clip");
  corruptOnce.add(cat);
  const dest = join(dir, "clips");
  const synced = await sync(lock, dest, {
    origin: origin(),
    cacheDir: join(dir, "user-cache"),
    quiet: true,
    retry,
  });
  expect(synced).toMatchObject({ files: 2, blobs: 2, fetched: 2, linked: 2, removed: 0 });
  expect(gets.filter((g) => g === cat).length).toBe(2);
  expect(readFileSync(join(dest, "words/cat.mp3"), "utf8")).toBe("cat clip");

  const again = await sync(lock, dest, {
    origin: origin(),
    cacheDir: join(dir, "user-cache"),
    quiet: true,
  });
  expect(again).toMatchObject({ fetched: 0, linked: 0, already: 2 });

  expect(await verify(lock, dest)).toEqual({ files: 2, ok: 2, missing: [], bad: [] });
  rmSync(join(dest, "words/dog.mp3"));
  const v = await verify(lock, dest);
  expect(v.missing).toEqual(["words/dog.mp3"]);
});

test("a failure rejects the promise with the reason", async () => {
  const lock = join(dir, "missing.lock");
  writeFileSync(lock, `${sha("nowhere")} 7 x.mp3\n`);
  await expect(
    sync(lock, join(dir, "never"), {
      origin: { http: { baseUrl: `http://127.0.0.1:${server.port}/blobs` } },
      cacheDir: join(dir, "empty-cache"),
      quiet: true,
      retry,
    }),
  ).rejects.toThrow(/401/);
});
