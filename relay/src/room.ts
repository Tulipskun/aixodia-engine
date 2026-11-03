type Role = "engine" | "phone";

type Attachment = { role: Role | null; started?: number };

type EngineSlot = { started: number; version: string };

export class Room2 {
  constructor(private state: DurableObjectState, private env: unknown) {}

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
      return new Response("expected websocket", { status: 400 });
    }
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair) as [WebSocket, WebSocket];
    this.state.acceptWebSocket(server);
    server.serializeAttachment({ role: null } satisfies Attachment);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    try {
      const att = (ws.deserializeAttachment() ?? { role: null }) as Attachment;

      if (!att.role) {
        if (typeof message !== "string") return;
        let hello: unknown;
        try {
          hello = JSON.parse(message);
        } catch {
          return;
        }
        const role = (hello as { role?: unknown } | null)?.role;

        if (role === "engine") {
          const started = Number((hello as { started?: unknown }).started);
          if (!Number.isFinite(started)) return;
          const prev = await this.state.storage.get<EngineSlot>("engine").catch(() => undefined);
          if (prev && started < prev.started) {
            this.reject(ws, "superseded");
            return;
          }
          // A new engine means a new relay session, and every terminal stream
          // the phone opened belonged to the old one. The phone only re-dials
          // its shells when presence goes false then true, so announce the gap
          // here: replacing the engine used to send a second "true" to a phone
          // that already had "true", and the dead streams stayed on screen.
          if (prev) this.publishPresence(false);
          for (const other of this.state.getWebSockets()) {
            if (other === ws) continue;
            const oa = this.attachmentOf(other);
            if (oa?.role === "engine") this.reject(other, "superseded");
          }
          att.role = "engine";
          att.started = started;
          ws.serializeAttachment(att);
          await this.state.storage.put("engine", {
            started,
            version: String((hello as { version?: unknown }).version ?? ""),
          } satisfies EngineSlot).catch(() => {});
          this.publishPresence();
          return;
        }

        if (role === "phone") {
          const old = this.peer("phone");
          if (old && old !== ws) this.reject(old, "superseded-phone");
          att.role = "phone";
          ws.serializeAttachment(att);
          this.publishPresence();
          return;
        }
        return;
      }

      const other = this.peer(att.role === "engine" ? "phone" : "engine");
      if (other) {
        try {
          other.send(message);
        } catch (e) {
          console.error("relay forward failed", e);
        }
      }
    } catch (e) {
      console.error("relay webSocketMessage failed", e);
    }
  }

  async webSocketClose(ws: WebSocket) {
    await this.clear(ws);
  }

  async webSocketError(ws: WebSocket) {
    await this.clear(ws);
  }

  private async clear(ws: WebSocket) {
    try {
      const att = this.attachmentOf(ws);
      if (att?.role === "engine") {
        const slot = await this.state.storage.get<EngineSlot>("engine").catch(() => undefined);
        if (slot && slot.started === att.started) {
          await this.state.storage.delete("engine").catch(() => {});
        }
      }
      this.publishPresence();
    } catch (e) {
      console.error("relay clear failed", e);
      try {
        this.publishPresence();
      } catch {}
    }
  }

  private reject(ws: WebSocket, ctl: string) {
    try {
      ws.send(JSON.stringify({ ctl }));
    } catch (e) {
      console.error("relay reject send failed", e);
    }
    try {
      ws.close(1000, ctl);
    } catch (e) {
      console.error("relay reject close failed", e);
    }
  }

  private attachmentOf(ws: WebSocket): Attachment | null {
    try {
      return (ws.deserializeAttachment() ?? { role: null }) as Attachment;
    } catch {
      return null;
    }
  }

  // Only sockets that are still open count: a socket that has gone away but has
  // not yet been cleared by webSocketClose must not be able to block a new
  // registration (an open() on it throws and aborts the whole handler).
  private peer(role: Role): WebSocket | undefined {
    return this.state
      .getWebSockets()
      .find((s) => {
        try {
          if ((s as { readyState?: number }).readyState !== 1) return false;
          const a = this.attachmentOf(s);
          return a !== null && a.role === role;
        } catch {
          return false;
        }
      });
  }

  private publishPresence(engine: boolean = !!this.peer("engine")) {
    const phone = this.peer("phone");
    if (!phone) return;
    try {
      phone.send(JSON.stringify({ ctl: "presence", engine }));
    } catch (e) {
      console.error("relay publishPresence failed", e);
    }
  }
}
