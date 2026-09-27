// Foreground 時だけの WebSocket（ADR-0015 §3, §5）。
//
// - Browser の WebSocket は Authorization ヘッダを付けられないので、HTTP で取った一回限りの ticket を
//   Sec-WebSocket-Protocol で渡す（URL クエリはアクセスログに残りうるため使わない）。
// - ページが隠れたら自分から閉じる。Background の Browser は接続を維持できる保証がなく、半開きの接続が
//   残るとサーバーが「WS で届いた」と判断して Web Push を送らなくなるため。
// - 通知はヒント。接続・再接続のたびに onResync を呼び、状態の正である HTTP API から取り直す。

const PING_MS = 4 * 60 * 1000; // API Gateway の idle timeout(10分) より短く
const MAX_BACKOFF_MS = 30 * 1000;

export class Realtime {
  /**
   * @param {import("./api.js").Api} api
   * @param {{onEvent: (e: any) => void, onResync: () => void, onState: (s: "online"|"connecting"|"offline") => void}} h
   */
  constructor(api, h) {
    this.api = api;
    this.h = h;
    this.ws = null;
    this.backoff = 1000;
    this.timer = null;
    this.ping = null;
    this.wanted = false;
    document.addEventListener("visibilitychange", () => (document.hidden ? this.stop() : this.start()));
    addEventListener("pagehide", () => this.stop());
    addEventListener("online", () => this.start());
  }

  start() {
    if (document.hidden) return;
    this.wanted = true;
    if (this.ws && this.ws.readyState <= WebSocket.OPEN) return;
    clearTimeout(this.timer);
    this.connect();
  }

  stop() {
    this.wanted = false;
    clearTimeout(this.timer);
    clearInterval(this.ping);
    if (this.ws) {
      const ws = this.ws;
      this.ws = null;
      ws.onclose = null;
      ws.close(1000);
    }
    this.h.onState("offline");
  }

  async connect() {
    this.h.onState("connecting");
    let ticket;
    try {
      ticket = await this.api.wsTicket();
    } catch {
      return this.retry();
    }
    if (!this.wanted) return;
    const url = `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/ws`;
    const ws = new WebSocket(url, ["tsute.v1", `ticket.${ticket}`]);
    this.ws = ws;
    ws.onopen = () => {
      this.backoff = 1000;
      this.h.onState("online");
      clearInterval(this.ping);
      this.ping = setInterval(() => ws.readyState === WebSocket.OPEN && ws.send('{"action":"ping"}'), PING_MS);
      this.h.onResync();
    };
    ws.onmessage = (m) => {
      let e;
      try { e = JSON.parse(m.data); } catch { return; }
      this.h.onEvent(e);
    };
    ws.onclose = () => {
      clearInterval(this.ping);
      if (this.ws === ws) this.ws = null;
      this.retry();
    };
  }

  retry() {
    if (!this.wanted) return;
    this.h.onState("offline");
    const jitter = Math.random() * 1000;
    this.timer = setTimeout(() => this.connect(), this.backoff + jitter);
    this.backoff = Math.min(this.backoff * 2, MAX_BACKOFF_MS);
  }
}
