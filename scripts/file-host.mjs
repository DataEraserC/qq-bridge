#!/usr/bin/env node
// 只读文件托管（/files/<能力token>）的投放 CLI —— file-host 的**生产者**。
//
// 用法（在 qq-bridge 目录下执行）：
//   node scripts/file-host.mjs publish <本机文件> [--name 别名] [--token <32hex>]
//   node scripts/file-host.mjs list
//   node scripts/file-host.mjs remove <token>
//   node scripts/file-host.mjs url <token>
//
// 设计：桥接**没有上传端点**（写攻击面为零、也不受 HTTP 2MiB 请求体上限约束），
// 投放是纯文件系统操作：把文件复制进 state/hosted/<token>/。本 CLI 与桥接端点
// 共用 src/ext/file-host.js 的同一份目录约定（一 token 一文件、先写 .part 再 rename）。
//
// 打印出的 URL 里 token 就是门禁（128-bit 随机、与 consoleToken 零耦合）——
// 知道 URL 即有权取用，泄露半径只及该文件本身；下线用 remove 删目录。
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  listHostedFiles,
  publishHostedFile,
  removeHostedFile,
} from "../src/ext/file-host.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, "..");
const HOSTED_DIR = path.join(ROOT, "state", "hosted");
const CONFIG_PATH = path.join(ROOT, "config.json");

/** 控制台基址：只读本机配置里的 consolePort，不猜。 */
function consoleBase() {
  let port = 3100;
  try {
    const cfg = JSON.parse(fs.readFileSync(CONFIG_PATH, "utf8"));
    if (Number(cfg.consolePort) > 0) port = Number(cfg.consolePort);
  } catch {
    // 没有 config.json 就用默认端口（与 loadConfig 一致），不报错。
  }
  return `http://127.0.0.1:${port}`;
}

function fmtSize(n) {
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)}MiB`;
  if (n >= 1024) return `${Math.round(n / 1024)}KB`;
  return `${n}B`;
}

function usage(code = 0) {
  const text = `用法：
  node scripts/file-host.mjs publish <文件> [--name 别名] [--token <32hex>]
  node scripts/file-host.mjs list
  node scripts/file-host.mjs remove <token>
  node scripts/file-host.mjs url <token>

投放是文件系统操作（桥接无上传端点）：复制进 state/hosted/<token>/ 后即可用
\`<控制台基址>/files/<token>\` 取用；知道 URL 即有权取用，下线用 remove。`;
  (code === 0 ? console.log : console.error)(text);
  process.exit(code);
}

const argv = process.argv.slice(2);
const flags = new Map();
const positional = [];
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a.startsWith("--")) {
    const [k, v] = a.slice(2).split("=");
    if (v !== undefined) flags.set(k, v);
    else if (argv[i + 1] && !argv[i + 1].startsWith("--")) flags.set(k, argv[++i]);
    else flags.set(k, true);
  } else positional.push(a);
}

const [command, ...rest] = positional;

try {
  if (command === "publish") {
    const source = rest[0];
    if (!source) usage(1);
    const token = flags.get("token");
    const name = flags.get("name");
    const result = await publishHostedFile(HOSTED_DIR, source, {
      token: token === true ? undefined : token,
      name: name === true ? undefined : name,
    });
    console.log(`${consoleBase()}/files/${result.token}`);
    console.log(
      `  ${result.fileName}（${fmtSize(result.size)}）→ state/hosted/${result.token}/`,
    );
  } else if (command === "list") {
    const items = await listHostedFiles(HOSTED_DIR);
    if (!items.length) {
      console.log("（没有托管中的文件）");
    } else {
      for (const item of items) {
        console.log(`${item.token}  ${fmtSize(item.size).padStart(8)}  ${item.fileName}`);
      }
    }
  } else if (command === "remove") {
    const token = rest[0];
    if (!token) usage(1);
    const removed = await removeHostedFile(HOSTED_DIR, token);
    console.log(removed ? `已下线 ${token}` : `没有这个 token：${token}`);
    if (!removed) process.exit(1);
  } else if (command === "url") {
    const token = rest[0];
    if (!token) usage(1);
    if (!/^[0-9a-f]{32}$/.test(token)) {
      console.error("token 必须是 32 位小写十六进制");
      process.exit(1);
    }
    console.log(`${consoleBase()}/files/${token}`);
  } else {
    usage(command ? 1 : 0);
  }
} catch (error) {
  console.error(`❌ ${error?.message ?? error}`);
  process.exit(1);
}
