// 一次性开发工具：通过 CDP 注入会话并截取列表页 / 聊天页。
// 用法：node /tmp/web-shot.mjs http://localhost:18080 /mnt/e/rustapp/temp

const BASE = process.argv[2] ?? 'http://localhost:18080';
const OUT = process.argv[3] ?? '/mnt/e/rustapp/temp';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const reg = await (
  await fetch(`${BASE}/api/auth/register`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      handle: `shot${Date.now()}`,
      password: 'password123',
      kind: 'web',
      device_name: 'Chrome · Windows',
    }),
  })
).json();

const room = await (
  await fetch(`${BASE}/api/rooms`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${reg.token}` },
    body: JSON.stringify({ name: '周末造个 App' }),
  })
).json();

for (const text of ['你好，网页端', '这是第二条消息，检查一下气泡和间距。']) {
  await fetch(`${BASE}/api/rooms/${room.id}/messages`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${reg.token}` },
    body: JSON.stringify({ client_msg_id: `shot-${Math.random()}`, text }),
  });
}

const targets = await (await fetch('http://127.0.0.1:9223/json/list')).json();
const page = targets.find((t) => t.type === 'page');
if (!page) throw new Error('no page target');

const ws = new WebSocket(page.webSocketDebuggerUrl);
const pending = new Map();
let seq = 0;

ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) {
    pending.get(m.id)(m);
    pending.delete(m.id);
  }
};

const call = (method, params = {}) => {
  const id = ++seq;
  ws.send(JSON.stringify({ id, method, params }));
  return new Promise((resolve) => pending.set(id, resolve));
};

await new Promise((r) => (ws.onopen = r));
await call('Page.enable');
await call('Emulation.setDeviceMetricsOverride', {
  width: 1280,
  height: 800,
  deviceScaleFactor: 1,
  mobile: false,
});

await call('Page.navigate', { url: `${BASE}/` });
await sleep(1200);

const session = { token: reg.token, deviceId: reg.device_id, user: reg.user };
await call('Runtime.evaluate', {
  expression: `localStorage.setItem('aox.session', ${JSON.stringify(JSON.stringify(session))}); 'ok'`,
});
await call('Page.reload');
await sleep(1800);

async function shot(name) {
  const res = await call('Page.captureScreenshot', { format: 'png' });
  const fs = await import('node:fs');
  fs.writeFileSync(`${OUT}/${name}.png`, Buffer.from(res.result.data, 'base64'));
  console.log('wrote', `${OUT}/${name}.png`);
}

await shot('web-list');

await call('Page.navigate', { url: `${BASE}/r/${room.id}` });
await sleep(2000);
await shot('web-chat');

ws.close();
process.exit(0);
