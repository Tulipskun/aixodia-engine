package terminal

import (
	"bufio"
	"encoding/json"
	"errors"
	"net"
	"os"
	"strings"
	"sync"
	"time"

	"aixodia-engine/io/wsock"
	"github.com/pion/webrtc/v4"
)

const (
	rtcMaxSignalBytes = 1 << 20
	rtcGatherTimeout  = 15 * time.Second
	rtcFrameChunk     = 16 << 10
)

type rtcSignal struct {
	Ch   string `json:"ch"`
	Type string `json:"type"`
	SID  string `json:"sid"`
	SDP  string `json:"sdp,omitempty"`
	Cols int    `json:"cols,omitempty"`
	Rows int    `json:"rows,omitempty"`
}

// RTCServer accepts authenticated signaling messages from the existing relay
// session, then moves the terminal stream onto a direct WebRTC DataChannel.
// Only offer/answer signaling crosses the relay; PTY data does not.
type RTCServer struct {
	term  *Server
	send  func([]byte) error
	mu    sync.Mutex
	peers map[string]*webrtc.PeerConnection
}

func NewRTCServer(term *Server, send func([]byte) error) *RTCServer {
	return &RTCServer{term: term, send: send, peers: make(map[string]*webrtc.PeerConnection)}
}

func (s *RTCServer) Handle(raw []byte) error {
	if len(raw) == 0 || len(raw) > rtcMaxSignalBytes {
		return errors.New("rtc signaling message size rejected")
	}
	var sig rtcSignal
	if err := json.Unmarshal(raw, &sig); err != nil {
		return err
	}
	if sig.Ch != "rtc" || sig.Type != "offer" || strings.TrimSpace(sig.SID) == "" || len(sig.SID) > 128 || len(sig.SDP) == 0 || len(sig.SDP) > rtcMaxSignalBytes {
		return errors.New("invalid rtc offer")
	}
	go s.answer(sig)
	return nil
}

func (s *RTCServer) answer(sig rtcSignal) {
	cfg := webrtc.Configuration{ICEServers: []webrtc.ICEServer{{URLs: []string{"stun:stun.l.google.com:19302"}}}}
	if urls := strings.TrimSpace(os.Getenv("AIXODIA_TURN_URL")); urls != "" {
		server := webrtc.ICEServer{URLs: strings.Split(urls, ",")}
		if username := os.Getenv("AIXODIA_TURN_USERNAME"); username != "" {
			server.Username = username
			server.Credential = os.Getenv("AIXODIA_TURN_CREDENTIAL")
			server.CredentialType = webrtc.ICECredentialTypePassword
		}
		cfg.ICEServers = append(cfg.ICEServers, server)
	}
	pc, err := webrtc.NewPeerConnection(cfg)
	if err != nil {
		s.sendSignal(rtcSignal{Ch: "rtc", Type: "error", SID: sig.SID, SDP: err.Error()})
		return
	}
	s.mu.Lock()
	if old := s.peers[sig.SID]; old != nil {
		_ = old.Close()
	}
	s.peers[sig.SID] = pc
	s.mu.Unlock()

	remove := func() {
		s.mu.Lock()
		if s.peers[sig.SID] == pc {
			delete(s.peers, sig.SID)
		}
		s.mu.Unlock()
		_ = pc.Close()
	}
	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed {
			remove()
		}
	})
	pc.OnDataChannel(func(dc *webrtc.DataChannel) {
		dc.OnOpen(func() {
			client, server := net.Pipe()
			if err := s.term.Attach(server, sig.SID, sig.Cols, sig.Rows); err != nil {
				_ = dc.SendText(mustRTCJSON(map[string]any{"type": "error", "error": err.Error()}))
				_ = client.Close()
				_ = server.Close()
				return
			}
			dc.OnMessage(func(msg webrtc.DataChannelMessage) {
				op := byte(wsock.OpBinary)
				if msg.IsString {
					op = wsock.OpText
				}
				if err := wsock.WriteFrame(client, op, msg.Data); err != nil {
					_ = client.Close()
					_ = dc.Close()
				}
			})
			dc.OnClose(func() {
				_ = client.Close()
				remove()
			})
			go pumpRTCFrames(client, dc)
		})
	})

	offer := webrtc.SessionDescription{Type: webrtc.SDPTypeOffer, SDP: sig.SDP}
	if err := pc.SetRemoteDescription(offer); err != nil {
		s.sendSignal(rtcSignal{Ch: "rtc", Type: "error", SID: sig.SID, SDP: "invalid offer"})
		remove()
		return
	}
	answer, err := pc.CreateAnswer(nil)
	if err != nil {
		s.sendSignal(rtcSignal{Ch: "rtc", Type: "error", SID: sig.SID, SDP: "answer creation failed"})
		remove()
		return
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(answer); err != nil {
		s.sendSignal(rtcSignal{Ch: "rtc", Type: "error", SID: sig.SID, SDP: "local description failed"})
		remove()
		return
	}
	select {
	case <-gathered:
	case <-time.After(rtcGatherTimeout):
		s.sendSignal(rtcSignal{Ch: "rtc", Type: "error", SID: sig.SID, SDP: "ICE gathering timed out"})
		remove()
		return
	}
	local := pc.LocalDescription()
	if local == nil {
		remove()
		return
	}
	s.sendSignal(rtcSignal{Ch: "rtc", Type: "answer", SID: sig.SID, SDP: local.SDP})
}

func (s *RTCServer) sendSignal(sig rtcSignal) {
	raw, err := json.Marshal(sig)
	if err == nil && s.send != nil {
		_ = s.send(raw)
	}
}

func (s *RTCServer) Close() {
	s.mu.Lock()
	peers := make([]*webrtc.PeerConnection, 0, len(s.peers))
	for _, pc := range s.peers {
		peers = append(peers, pc)
	}
	s.peers = make(map[string]*webrtc.PeerConnection)
	s.mu.Unlock()
	for _, pc := range peers {
		_ = pc.Close()
	}
}

// pumpRTCFrames adapts the existing terminal WebSocket frame stream to the
// message-oriented DataChannel. PTY bytes remain binary and control events text.
func pumpRTCFrames(conn net.Conn, dc *webrtc.DataChannel) {
	reader := bufio.NewReader(conn)
	for {
		op, payload, err := wsock.ReadFrame(reader)
		if err != nil {
			_ = conn.Close()
			return
		}
		switch op {
		case wsock.OpText:
			if err = dc.SendText(string(payload)); err != nil {
				_ = conn.Close()
				return
			}
		case wsock.OpBinary:
			for start := 0; start < len(payload); start += rtcFrameChunk {
				end := start + rtcFrameChunk
				if end > len(payload) {
					end = len(payload)
				}
				if err = dc.Send(payload[start:end]); err != nil {
					_ = conn.Close()
					return
				}
			}
		case wsock.OpClose:
			_ = dc.Close()
			_ = conn.Close()
			return
		}
	}
}

func mustRTCJSON(v any) string {
	raw, _ := json.Marshal(v)
	return string(raw)
}
