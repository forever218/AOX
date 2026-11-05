// 待办冒烟：B 加一条，A 的 WS 立刻收到全量 todos；勾选/删除同样广播。
const BASE = 'https://chat.2am.top';
const WSURL = 'wss://chat.2am.top/ws';

async function call(method, path, token, body) {
  const headers = {};
  if (token) headers.authorization = 'Bearer ' + token;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const r = await fetch(BASE + path, {
    method, headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let j = null;
  try { j = await r.json(); } catch {}
  return { status: r.status, body: j };
}

function connect(token, deviceId) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(WSURL);
    const frames = [];
    const waiters = [];
    ws.onmessage = (ev) => {
      const f = JSON.parse(ev.data);
      frames.push(f);
      for (const w of waiters.slice()) {
        if (w.pred(f)) { waiters.splice(waiters.indexOf(w), 1); w.resolve(f); }
      }
    };
    ws.onerror = () => reject(new Error('ws error'));
    ws.onopen = () => ws.send(JSON.stringify({
      v: 1, id: 'h' + Math.random(), ts: Date.now(), type: 'hello',
      auth: { token, device_id: deviceId, proto: 1 },
      device: { kind: 'phone', name: 'todo-smoke', app_version: '1.0.0' },
      resume: {},
    }));
    resolve({
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
            rej(new Error(`timeout ${what}; got ${JSON.stringify(frames.map((f) => f.type))}`));
          }, ms);
        });
      },
      close() { try { ws.close(); } catch {} },
    });
  });
}

const tag = Date.now().toString().slice(-6);
async function newUser(name) {
  const r = await call('POST', '/api/auth/register', null, { handle: `${name}${tag}`, password: 'password123' });
  if (r.status !== 200) throw new Error('register: ' + JSON.stringify(r));
  return { token: r.body.token, deviceId: r.body.device_id };
}

const a = await newUser('tda');
const b = await newUser('tdb');
const room = await call('POST', '/api/rooms', a.token, { name: '待办冒烟' });
const roomId = room.body.id;
const inv = await call('POST', `/api/rooms/${roomId}/invites`, a.token);
await call('POST', '/api/rooms/join', b.token, { code: inv.body.code });

const sa = await connect(a.token, a.deviceId);
await sa.wait((f) => f.type === 'hello_ok', 8000, 'A hello');
const sb = await connect(b.token, b.deviceId);
await sb.wait((f) => f.type === 'hello_ok', 8000, 'B hello');

// B 加一条：A 应该收到全量帧
const created = await call('POST', `/api/rooms/${roomId}/todos`, b.token, { text: '把设置页拆成三栏' });
if (created.status !== 200) throw new Error('create: ' + JSON.stringify(created));
const f1 = await sa.wait((f) => f.type === 'todos' && f.items.length === 1, 8000, 'todos(1)');
console.log('broadcast add:', f1.items[0].text, '| by', f1.items[0].created_by_name);

// A 勾完成：B 收到 done=true
const tid = created.body.id;
const upd = await call('PATCH', `/api/rooms/${roomId}/todos/${tid}`, a.token, { done: true });
if (upd.status !== 200) throw new Error('update: ' + JSON.stringify(upd));
const f2 = await sb.wait((f) => f.type === 'todos' && f.items.length === 1 && f.items[0].done === true, 8000, 'todos(done)');
console.log('broadcast toggle:', f2.items[0].done);

// 删除：两边都收到空列表
const del = await call('DELETE', `/api/rooms/${roomId}/todos/${tid}`, b.token);
if (del.status !== 200) throw new Error('delete: ' + JSON.stringify(del));
const f3 = await sa.wait((f) => f.type === 'todos' && f.items.length === 0, 8000, 'todos(empty)');
console.log('broadcast delete:', f3.items.length);

sa.close();
sb.close();
await call('POST', '/api/account/delete', a.token, { password: 'password123' });
await call('POST', '/api/account/delete', b.token, { password: 'password123' });
console.log('TODO SMOKE OK');
process.exit(0);
