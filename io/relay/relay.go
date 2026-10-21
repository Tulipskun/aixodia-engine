// Package relay keeps one outbound WebSocket to the Cloudflare relay and makes
// the rest of the engine run behind it: the phone hub, the terminal, and the
// HTTP API all keep their existing code and see ordinary connections and
// requests. The relay is transparent, so this package owns only the transport
// mapping, never the meaning of a chat frame.
package relay

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"

	"aixodia-engine/io/mobile"
	"aixodia-engine/io/terminal"
	"aixodia-engine/io/wsock"
)

// Binary frame channel byte. Terminal traffic rides binary frames prefixed with
// this byte and a two-byte stream id, so pty output is not base64-inflated.
const ChTerm = 0x02

// ErrSuperseded means a newer engine took the room; this process must exit.
var ErrSuperseded = errors.New("relay: superseded by a newer engine")

// Config is what the relay needs from main.
type Config struct {
	URL string // wss://host/room/<name>
	// Token is the relay's shared secret, checked by the Worker.
	Token string
	// APIToken is what the engine's own mux checks on the in-process request.
	// They are the same secret by default, but they need not be: the relay
	// token can be rotated without touching the phone's API token.
	APIToken string
	Started  int64
	Version  string

	Hub  *mobile.Hub
	Chat mobile.ChatFunc
	API  http.Handler
	Term *terminal.Server

	OnConnect    func()
	OnDisconnect func()
	Logf         func(format string, args ...any)
}

func (c Config) logf(format string, args ...any) {
	if c.Logf != nil {
		c.Logf(format, args...)
		return
	}
	log.Printf(format, args...)
}

// Run dials the relay and keeps it dialled until ctx ends or a newer engine
// takes the room. A blip is measured in seconds, not half a minute: the phone
// has an open room but no engine in it until this process is back, and the
// handshake itself already takes a couple of seconds on a phone network. The
// cap is therefore short, and a connection that stayed up for ten seconds
// counts as healthy, so the next blip starts again from one second.
func Run(ctx context.Context, cfg Config) error {
	backoff := time.Second
	for {
		began := time.Now()
		err := runOnce(ctx, cfg)
		if errors.Is(err, ErrSuperseded) {
			return err
		}
		if ctx.Err() != nil {
			return nil
		}
		if err != nil {
			cfg.logf("relay: %v", err)
		}
		if time.Since(began) > 10*time.Second {
			backoff = time.Second
		}
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(backoff):
		}
		if backoff < 5*time.Second {
			backoff *= 2
		}
	}
}

func runOnce(ctx context.Context, cfg Config) error {
	h := http.Header{}
	h.Set("Authorization", "Bearer "+cfg.Token)
	c, err := wsock.Dial(ctx, cfg.URL, h)
	if err != nil {
		return err
	}
	defer c.Close()
	// The relay answers protocol pings (Cloudflare terminates the WebSocket at
	// the edge), so a silent read is proof the socket is gone. Without this a
	// half-open socket is only noticed when TCP keepalive finally gives up,
	// which is minutes — the phone shows the engine offline that whole time.
	c.SetLiveness(60 * time.Second)
	stop := make(chan struct{})
	defer close(stop)
	go func() {
		select {
		case <-ctx.Done():
			c.Close()
		case <-stop:
		}
	}()

	hello, err := json.Marshal(map[string]any{
		"role": "engine", "started": cfg.Started, "version": cfg.Version,
	})
	if err != nil {
		return err
	}
	if err := c.WriteMessage(wsock.OpText, hello); err != nil {
		return err
	}

	if cfg.OnConnect != nil {
		cfg.OnConnect()
	}
	defer func() {
		if cfg.OnDisconnect != nil {
			cfg.OnDisconnect()
		}
	}()

	s := &session{cfg: cfg, c: c, terms: map[uint16]*termStream{}, uploads: map[string]*bytes.Buffer{}}
	s.rtc = terminal.NewRTCServer(cfg.Term, func(raw []byte) error { return c.WriteMessage(wsock.OpText, raw) })
	defer s.close()

	pingCtx, stopPings := context.WithCancel(ctx)
	defer stopPings()
	go s.ping(pingCtx)

	for {
		op, payload, err := c.ReadMessage()
		if err != nil {
			return err
		}
		switch op {
		case wsock.OpText:
			if err := s.text(payload); err != nil {
				return err
			}
		case wsock.OpBinary:
			s.binary(payload)
		}
	}
}

type termStream struct {
	id   string
	conn *pipeConn
}

type session struct {
	cfg Config
	c   *wsock.Client

	mu      sync.Mutex
	chat    *pipeConn
	terms   map[uint16]*termStream
	uploads map[string]*bytes.Buffer
	rtc     *terminal.RTCServer
}

func (s *session) close() {
	s.mu.Lock()
	chat := s.chat
	terms := make([]*pipeConn, 0, len(s.terms))
	for _, t := range s.terms {
		terms = append(terms, t.conn)
	}
	s.terms = map[uint16]*termStream{}
	s.chat = nil
	rtc := s.rtc
	s.rtc = nil
	s.mu.Unlock()
	if rtc != nil { rtc.Close() }
	if chat != nil {
		chat.Close()
	}
	for _, t := range terms {
		t.Close()
	}
}

func (s *session) ping(ctx context.Context) {
	t := time.NewTicker(20 * time.Second)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			if err := s.c.WriteMessage(wsock.OpPing, nil); err != nil {
				return
			}
		}
	}
}

type envelope struct {
	Ch  string `json:"ch"`
	Ctl string `json:"ctl"`
	RID string `json:"rid"`
	M   string `json:"m"`
	// P is raw JSON because the two channels use it differently: an rpc
	// carries a path as a string, a terminal frame carries a control object
	// ({"type":"resize",...}). A string field cannot hold the second one, and
	// a frame that does not parse is dropped on the floor.
	P    json.RawMessage   `json:"p"`
	B    string            `json:"b"`
	More bool              `json:"more"`
	H    map[string]string `json:"h"`
	ID   string            `json:"id"`
	SID  int               `json:"sid"`
	Act  string            `json:"act"`
	Cols int               `json:"cols"`
	Rows int               `json:"rows"`
	K    string            `json:"k"`
}

// pString returns the payload as a string when it was sent as one, and as its
// raw JSON otherwise.
func (e envelope) pString() string {
	if len(e.P) == 0 {
		return ""
	}
	if e.P[0] == '"' {
		var out string
		if err := json.Unmarshal(e.P, &out); err == nil {
			return out
		}
	}
	return string(e.P)
}

func (s *session) text(payload []byte) error {
	var e envelope
	if err := json.Unmarshal(payload, &e); err != nil {
		s.cfg.logf("relay: unreadable frame: %v", err)
		return nil
	}
	if e.Ctl != "" {
		if e.Ctl == "superseded" {
			return ErrSuperseded
		}
		// presence and any future note from the relay itself: nothing to do.
		return nil
	}
	switch e.Ch {
	case "", "chat":
		return s.chatConn().Feed(wsock.OpText, payload)
	case "ctl":
		s.control(e)
	case "term":
		s.term(e)
	case "rtc":
		if s.rtc != nil {
			if err := s.rtc.Handle(payload); err != nil { s.cfg.logf("relay: rejected rtc signaling: %v", err) }
		}
	case "rpc":
		// Chunks arrive in order on one socket; buffering them here rather
		// than in the goroutine keeps a later chunk from overtaking an earlier
		// one. Only the dispatch is asynchronous.
		if e.More {
			s.appendChunk(e)
			return nil
		}
		go s.rpc(e)
	default:
		s.cfg.logf("relay: unknown channel %q", e.Ch)
	}
	return nil
}

func (s *session) binary(payload []byte) {
	if len(payload) < 3 || payload[0] != ChTerm {
		return
	}
	sid := binary.BigEndian.Uint16(payload[1:3])
	s.mu.Lock()
	st := s.terms[sid]
	s.mu.Unlock()
	if st == nil {
		return
	}
	_ = st.conn.Feed(wsock.OpBinary, payload[3:])
}

func (s *session) control(e envelope) {
	switch e.Act {
	case "open":
		sid := uint16(e.SID)
		s.mu.Lock()
		// A phone starts its stream ids over when its process restarts, while
		// this relay session only forgets ids when the engine reconnects. A
		// stale entry for the same sid must therefore give way to the new
		// open: returning early left the phone waiting for a ready frame that
		// could never arrive.
		old := s.terms[sid]
		pc := newPipeConn(s.termSink(sid, e.ID))
		s.terms[sid] = &termStream{id: e.ID, conn: pc}
		s.mu.Unlock()
		if old != nil {
			old.conn.Close()
		}
		if err := s.cfg.Term.Attach(pc, e.ID, e.Cols, e.Rows); err != nil {
			s.termError(e.ID, sid, err)
			s.mu.Lock()
			delete(s.terms, sid)
			s.mu.Unlock()
			pc.Close()
		}
	case "close":
		s.mu.Lock()
		st := s.terms[uint16(e.SID)]
		delete(s.terms, uint16(e.SID))
		s.mu.Unlock()
		if st != nil {
			st.conn.Close()
		}
	case "bye":
		// The phone is closing the app: stop every turn it started, but keep
		// the relay socket so the next launch reuses it.
		_ = s.chatConn().Feed(wsock.OpText, []byte(`{"type":"cancel"}`))
	}
}

func (s *session) term(e envelope) {
	s.mu.Lock()
	st := s.terms[uint16(e.SID)]
	s.mu.Unlock()
	if st == nil {
		return
	}
	switch e.K {
	case "b64":
		raw, err := base64.StdEncoding.DecodeString(e.pString())
		if err != nil {
			return
		}
		_ = st.conn.Feed(wsock.OpBinary, raw)
	default:
		_ = st.conn.Feed(wsock.OpText, []byte(e.P))
	}
}

func (s *session) termError(id string, sid uint16, err error) {
	out, _ := json.Marshal(map[string]any{
		"ch": "term", "id": id, "sid": sid, "k": "json",
		"p": map[string]any{"type": "error", "error": err.Error()},
	})
	_ = s.c.WriteMessage(wsock.OpText, out)
}

func (s *session) chatConn() *pipeConn {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.chat != nil {
		return s.chat
	}
	pc := newPipeConn(s.chatSink)
	s.chat = pc
	go s.cfg.Hub.ServeConn(pc, s.cfg.Chat)
	return pc
}

func (s *session) chatSink(op byte, payload []byte) error {
	if op == wsock.OpClose {
		return nil
	}
	if op != wsock.OpText {
		return s.c.WriteMessage(op, payload)
	}
	var m map[string]json.RawMessage
	if err := json.Unmarshal(payload, &m); err != nil {
		return s.c.WriteMessage(wsock.OpText, payload)
	}
	m["ch"], _ = json.Marshal("chat")
	out, err := json.Marshal(m)
	if err != nil {
		return err
	}
	return s.c.WriteMessage(wsock.OpText, out)
}

func (s *session) termSink(sid uint16, id string) func(byte, []byte) error {
	return func(op byte, payload []byte) error {
		switch op {
		case wsock.OpBinary:
			buf := make([]byte, 3+len(payload))
			buf[0] = ChTerm
			binary.BigEndian.PutUint16(buf[1:3], sid)
			copy(buf[3:], payload)
			return s.c.WriteMessage(wsock.OpBinary, buf)
		case wsock.OpText:
			out, err := json.Marshal(map[string]any{
				"ch": "term", "id": id, "sid": sid, "k": "json", "p": json.RawMessage(payload),
			})
			if err != nil {
				return err
			}
			return s.c.WriteMessage(wsock.OpText, out)
		}
		return nil
	}
}

// appendChunk buffers one piece of a body that arrives over several frames.
func (s *session) appendChunk(e envelope) {
	chunk, err := base64.StdEncoding.DecodeString(e.B)
	if err != nil {
		return
	}
	s.mu.Lock()
	buf := s.uploads[e.RID]
	if buf == nil {
		buf = &bytes.Buffer{}
		s.uploads[e.RID] = buf
	}
	buf.Write(chunk)
	s.mu.Unlock()
}

// rpc runs one phone request through the HTTP mux in-process. A body larger than
// one frame arrives as base64 chunks flagged `more`; the last chunk dispatches.
func (s *session) rpc(e envelope) {
	reply := func(status int, hdr http.Header, body []byte, failure string) {
		out := map[string]any{"ch": "rpc", "rid": e.RID, "s": status}
		if failure != "" {
			out["e"] = failure
		}
		if len(hdr) > 0 {
			flat := map[string]string{}
			for k, vs := range hdr {
				if len(vs) > 0 {
					flat[k] = vs[0]
				}
			}
			out["h"] = flat
		}
		if body != nil {
			out["b"] = base64.StdEncoding.EncodeToString(body)
		}
		data, err := json.Marshal(out)
		if err != nil {
			return
		}
		if err := s.c.WriteMessage(wsock.OpText, data); err != nil {
			s.cfg.logf("relay: rpc reply: %v", err)
		}
	}

	chunk, err := base64.StdEncoding.DecodeString(e.B)
	if err != nil {
		reply(http.StatusBadRequest, nil, nil, "body is not base64")
		return
	}
	body := chunk
	s.mu.Lock()
	if buf := s.uploads[e.RID]; buf != nil {
		buf.Write(chunk)
		body = buf.Bytes()
		delete(s.uploads, e.RID)
	}
	s.mu.Unlock()

	if s.cfg.API == nil {
		reply(http.StatusNotImplemented, nil, nil, "no api behind the relay")
		return
	}
	method := e.M
	if method == "" {
		method = http.MethodGet
	}
	path := e.pString()
	if path == "" {
		path = "/"
	}
	if !strings.HasPrefix(path, "/") {
		path = "/" + path
	}
	req, err := http.NewRequestWithContext(context.Background(), method, path, bytes.NewReader(body))
	if err != nil {
		reply(http.StatusBadRequest, nil, nil, err.Error())
		return
	}
	for k, v := range e.H {
		req.Header.Set(k, v)
	}
	// The relay already checked the bearer at the phone's handshake; the mux
	// checks it again, so the token travels with the in-process request.
	apiToken := s.cfg.APIToken
	if apiToken == "" {
		apiToken = s.cfg.Token
	}
	req.Header.Set("Authorization", "Bearer "+apiToken)

	func() {
		defer func() {
			if r := recover(); r != nil {
				reply(http.StatusInternalServerError, nil, nil, fmt.Sprint(r))
			}
		}()
		rec := newRecorder()
		s.cfg.API.ServeHTTP(rec, req)
		reply(rec.status(), rec.Header(), rec.body.Bytes(), "")
	}()
}

// recorder is the smallest http.ResponseWriter the relay needs: a status, the
// headers, and the body. httptest.ResponseRecorder would do, but it drags the
// test package into the binary.
type recorder struct {
	hdr   http.Header
	code  int
	body  bytes.Buffer
	flush bool
}

func newRecorder() *recorder { return &recorder{hdr: http.Header{}, code: http.StatusOK} }

func (r *recorder) Header() http.Header { return r.hdr }

func (r *recorder) WriteHeader(code int) { r.code = code }

func (r *recorder) Write(p []byte) (int, error) { return r.body.Write(p) }

func (r *recorder) Flush() { r.flush = true }

func (r *recorder) status() int {
	if r.code == 0 {
		return http.StatusOK
	}
	return r.code
}

var _ net.Conn = (*pipeConn)(nil)
