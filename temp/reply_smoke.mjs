// 验证服务端会不会把 reply_to 展开进 msg_new —— 宿主靠这个字段才知道该去哪里取原文。

const BASE = 'https://chat.2am.top';
const WSURL = 'wss://chat.2am.top/ws';

async function call(method, path, token, body) {
  const headers = {};
  if (token) headers.authorization = 'Bearer ' + token;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const r = await fetch(BASE + path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let j = null;
  try { j = await r.json(); } catch {}
  return { status: r.status, body: j };
}

function connect(token, deviceId, kind) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(WSURL);
    const frames = [];
    const waiters = [];
    ws.onmessage = (ev) => {
      const f = JSON.parse(ev.data);
      frames.push(f);
      for (const w of waiters.slice()) {
        if (w.pred(f)) {
          waiters.splice(waiters.indexOf(w), 1);
          w.resolve(f);
        }
      }
    };
    ws.onerror = () => reject(new Error('ws error'));
    ws.onopen = () => ws.send(JSON.stringify({
      v: 1, id: 'h' + Math.random(), ts: Date.now(), type: 'hello',
      auth: { token, device_id: deviceId, proto: 1 },
      device: { kind, name: 'reply-smoke', app_version: '1.0.0' },
      resume: {},
    }));
    const api = {
      ws, frames,
      wait(pred, ms = 8000, what = 'frame') {
        const hit = frames.find(pred);
        if (hit) return Promise.resolve(hit);
        return new Promise((res, rej) => {
          const w = { pred, resolve: res };
          waiters.push(w);
          setTimeout(() => {
            const i = waiters.indexOf(w);
            if (i >= 0) waiters.splice(i, 1);
            rej(new Error(`timeout for ${what}; frames=${JSON.stringify(frames.map((f) => f.type))}`));
          }, ms);
        });
      },
      send(o) { ws.send(JSON.stringify(o)); },
      close() { try { ws.close(); } catch {} },
    };
    resolve(api);
  });
}

const tag = Date.now().toString().slice(-6);
async function newUser(name) {
  const handle = `${name}${tag}`;
  const r = await call('POST', '/api/auth/register', null, { handle, password: 'password123' });
  if (r.status !== 200) throw new Error('register: ' + JSON.stringify(r));
  return { handle, token: r.body.token, deviceId: r.body.device_id };
}

const a = await newUser('rpa');
const b = await newUser('rpb');
const room = await call('POST', '/api/rooms', a.token, { name: '引用冒烟' });
const roomId = room.body.id;
const inv = await call('POST', `/api/rooms/${roomId}/invites`, a.token);
await call('POST', '/api/rooms/join', b.token, { code: inv.body.code });

const sa = await connect(a.token, a.deviceId, 'phone');
await sa.wait((f) => f.type === 'hello_ok', 8000, 'A hello_ok');

// B 发一条"想法"
const sb = await connect(b.token, b.deviceId, 'phone');
await sb.wait((f) => f.type === 'hello_ok', 8000, 'B hello_ok');
sb.send({
  v: 1, id: 's1', ts: Date.now(), room: roomId, type: 'msg_send',
  client_msg_id: 'cb1', body: { kind: 'text', text: '我的想法：把设置页拆成三栏' },
});
const m1 = await sa.wait((f) => f.type === 'msg_new' && f.msg.seq === 1, 8000, 'seq1');

// A 引用它 + @aox
sa.send({
  v: 1, id: 's2', ts: Date.now(), room: roomId, type: 'msg_send',
  client_msg_id: 'ca1', reply_to: m1.msg.id,
  body: { kind: 'text', text: '@aox 这个想法怎么样，可行吗？' },
});
const m2 = await sa.wait((f) => f.type === 'msg_new' && f.msg.seq === 2, 8000, 'seq2');

console.log('seq2 reply_to =', JSON.stringify(m2.msg.reply_to));
if (!m2.msg.reply_to || m2.msg.reply_to.id !== m1.msg.id || m2.msg.reply_to.seq !== 1) {
  throw new Error('服务端没把引用展开进 msg_new');
}

// GET 也确认一次（手机重新进房走的是这条路）
const list = await call('GET', `/api/rooms/${roomId}/messages`, a.token);
const got = list.body.find((m) => m.seq === 2);
console.log('REST reply_to =', JSON.stringify(got.reply_to));
if (!got.reply_to || got.reply_to.id !== m1.msg.id) throw new Error('REST 里的引用丢了');

sa.close();
sb.close();
await call('POST', '/api/account/delete', a.token, { password: 'password123' });
await call('POST', '/api/account/delete', b.token, { password: 'password123' });
console.log('REPLY SMOKE OK');
process.exit(0);
