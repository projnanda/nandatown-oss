package main

// mcp.go — the MCP-gateway client for platform agents.
//
// The whole configuration is two env vars, both injected at deploy time
// (docs: MCP_GATEWAY_DESIGN.md, MCP gateway agent auth):
//
//	MCP_GATEWAY_URL   — where the platform's aggregating gateway lives
//	MCP_GATEWAY_TOKEN — this agent's own credential, sent as a Bearer header
//
// There is NO per-request credential to plumb: the platform resolves the
// calling user server-side from the `traceparent` this agent forwards, and
// authorizes the call only when this agent is a recorded participant of that
// flow. An agent that drops the header gets a clean 403, never misattribution.
//
// # Why the tool list is per request, not per process
//
// `tools/list` is query-aware (docs/MCP_TOOL_SEARCH_DESIGN.md): the gateway
// resolves the user's query from the flow named by `traceparent` and returns
// only the tools matching it, plus nasiko_search_tools. Called WITHOUT a
// traceparent — as a startup probe would — it returns the search meta-tool and
// nothing else.
//
// So an agent that discovers once at startup advertises exactly one tool
// forever. The model then searches, gets a tool name back, cannot call it (it
// was never advertised), and searches again until its round budget dies. Hence
// mcpSession: one per A2A request, seeded from a tools/list carrying that
// request's traceparent, and extended in place whenever the model searches — a
// nasiko_search_tools result carries full definitions, so what it finds becomes
// immediately callable.

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
)

// searchTool is the gateway's tool-search meta-tool. Always present in
// tools/list; its result carries the definitions of whatever it found.
const searchTool = "nasiko_search_tools"

// mcpClient holds the connection settings only — the tool set lives on
// mcpSession, because it is scoped to one request's query.
type mcpClient struct {
	url   string
	token string
	http  *http.Client
}

// newMCPClient returns a configured client, or nil when the platform didn't
// inject the gateway env (local runs, MCP disabled) — the agent then simply
// runs with its built-in tools only.
func newMCPClient() *mcpClient {
	url := os.Getenv("MCP_GATEWAY_URL")
	token := os.Getenv("MCP_GATEWAY_TOKEN")
	if url == "" || token == "" {
		// Logged rather than silent: "MCP is off" and "the image has no MCP
		// client" otherwise look identical in the agent's output.
		log.Printf("mcp: MCP_GATEWAY_URL/MCP_GATEWAY_TOKEN not set — running with built-in tools only")
		return nil
	}
	return &mcpClient{url: url, token: token, http: &http.Client{}}
}

// rpc posts one JSON-RPC request to the gateway. traceparent may be empty (the
// startup probe); when set it names the flow being served and is what makes
// tools/list query-aware.
func (c *mcpClient) rpc(ctx context.Context, traceparent string, body map[string]any) (json.RawMessage, error) {
	payload, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.url, bytes.NewReader(payload))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+c.token)
	if traceparent != "" {
		req.Header.Set("traceparent", traceparent)
	}

	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("gateway HTTP %d: %s", resp.StatusCode, string(raw))
	}
	var out struct {
		Result json.RawMessage `json:"result"`
		Error  *struct {
			Code    int             `json:"code"`
			Message string          `json:"message"`
			Data    json.RawMessage `json:"data"`
		} `json:"error"`
	}
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, fmt.Errorf("gateway response parse: %w", err)
	}
	if out.Error != nil {
		// Data carries the structured half of a gateway refusal: for an
		// AUTH_REQUIRED pause, the connector and the recorded HITL request id.
		// Keeping only the prose throws that away.
		detail := ""
		if len(out.Error.Data) > 0 && string(out.Error.Data) != "null" {
			detail = fmt.Sprintf(" (%s)", out.Error.Data)
		}
		return nil, fmt.Errorf("gateway error %d: %s%s", out.Error.Code, out.Error.Message, detail)
	}
	return out.Result, nil
}

// probe confirms the credential works before any flow exists. Best-effort: a
// failure only means the agent starts without knowing, not that requests fail.
//
// This is a health check, NOT discovery — without a traceparent the gateway
// returns only the search meta-tool, so the real tool set is fetched per
// request by openSession.
func (c *mcpClient) probe(ctx context.Context) {
	if c == nil {
		return
	}
	result, err := c.rpc(ctx, "", map[string]any{
		"jsonrpc": "2.0", "id": 1, "method": "tools/list",
	})
	if err != nil {
		log.Printf("mcp: gateway unreachable at startup, continuing: %v", err)
		return
	}
	var manifest struct {
		Tools []json.RawMessage `json:"tools"`
	}
	_ = json.Unmarshal(result, &manifest)
	log.Printf("mcp: gateway reachable (%d startup tools); tools resolved per request", len(manifest.Tools))
}

// mcpSession is the gateway tool set for ONE A2A request: query-scoped, and
// mutable because nasiko_search_tools adds to it mid-turn.
type mcpSession struct {
	client      *mcpClient
	traceparent string
	defs        []toolDef
	names       map[string]bool
}

// openSession fetches the tools matching this request's query. Returns nil when
// there is no client, so callers can use the result unconditionally.
func (c *mcpClient) openSession(ctx context.Context, traceparent string) *mcpSession {
	if c == nil {
		return nil
	}
	s := &mcpSession{client: c, traceparent: traceparent, names: map[string]bool{}}
	result, err := c.rpc(ctx, traceparent, map[string]any{
		"jsonrpc": "2.0", "id": 1, "method": "tools/list",
	})
	if err != nil {
		log.Printf("mcp: tools/list failed, continuing without gateway tools: %v", err)
		return s
	}
	s.absorb(result)
	return s
}

// absorb adds every {name, description, inputSchema} entry under `tools` to the
// advertised set, converting to the OpenAI function shape, and returns the names
// that were new. Shared by tools/list and nasiko_search_tools, which the gateway
// returns in the same shape.
func (s *mcpSession) absorb(result json.RawMessage) []string {
	var manifest struct {
		Tools []struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			InputSchema map[string]any `json:"inputSchema"`
		} `json:"tools"`
	}
	if err := json.Unmarshal(result, &manifest); err != nil {
		return nil
	}
	var added []string
	for _, t := range manifest.Tools {
		if t.Name == "" || s.names[t.Name] {
			continue
		}
		params := t.InputSchema
		if params == nil {
			// A tool may omit inputSchema when it takes no arguments, but the
			// chat-completions API rejects a function without one.
			params = map[string]any{"type": "object", "properties": map[string]any{}}
		}
		var def toolDef
		def.Type = "function"
		def.Function.Name = t.Name
		def.Function.Description = t.Description
		def.Function.Parameters = params
		s.defs = append(s.defs, def)
		s.names[t.Name] = true
		added = append(added, t.Name)
	}
	return added
}

// toolDefs are the gateway tools to advertise alongside the agent's built-ins.
func (s *mcpSession) toolDefs() []toolDef {
	if s == nil {
		return nil
	}
	return s.defs
}

// handles reports whether name is a gateway tool rather than a built-in.
func (s *mcpSession) handles(name string) bool {
	return s != nil && s.names[name]
}

// callTool executes one gateway tool on behalf of this request's flow.
//
// A nasiko_search_tools result is absorbed into the advertised set: without
// that the model can search, be told a tool exists, and still have no way to
// call it. Errors come back as content rather than errors so the model can
// recover — the same convention dispatchTool follows.
func (s *mcpSession) callTool(ctx context.Context, name, argsJSON string) string {
	if s == nil {
		return "The MCP gateway is not configured for this agent."
	}
	var args map[string]any
	if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
		return fmt.Sprintf("bad arguments: %v", err)
	}
	result, err := s.client.rpc(ctx, s.traceparent, map[string]any{
		"jsonrpc": "2.0", "id": 1, "method": "tools/call",
		"params": map[string]any{"name": name, "arguments": args},
	})
	if err != nil {
		return fmt.Sprintf("tool %s failed: %v", name, err)
	}

	if name == searchTool {
		added := s.absorb(result)
		if len(added) == 0 {
			return "No matching tools were found. Do not search again for the same thing; " +
				"answer from what you already have."
		}
		return fmt.Sprintf("These tools are now available and can be called directly: %s. "+
			"Call the one you need — do not search again.", strings.Join(added, ", "))
	}

	// Prefer the standard MCP text content; fall back to the raw result JSON so
	// structured-only results still reach the model.
	var content struct {
		Content []struct {
			Type string `json:"type"`
			Text string `json:"text"`
		} `json:"content"`
	}
	if err := json.Unmarshal(result, &content); err == nil {
		for _, c := range content.Content {
			if c.Type == "text" && c.Text != "" {
				return c.Text
			}
		}
	}
	return string(result)
}

// systemPromptHint returns a system-prompt fragment describing the
// connected-account tools, for the agent to append to its own prompt. Safe on a
// nil session.
//
// Advertising the tools is not enough: the agent's prompt enumerates its
// built-ins and names one of them as the general-purpose fallback, so the model
// answers "my issues"/"my emails" from a web search instead of the user's real
// account. The search→call flow also has to be spelled out, or the model treats
// nasiko_search_tools as the answer rather than the lookup.
func (s *mcpSession) systemPromptHint() string {
	if s == nil || len(s.defs) == 0 {
		return ""
	}
	direct := make([]string, 0, len(s.defs))
	for _, d := range s.defs {
		if d.Function.Name != searchTool {
			direct = append(direct, d.Function.Name)
		}
	}
	available := fmt.Sprintf("Right now only `%s` is listed; call it first to find the tool you need.", searchTool)
	if len(direct) > 0 {
		available = fmt.Sprintf("Already available: %s. Use `%s` only if none of those fit.",
			strings.Join(direct, ", "), searchTool)
	}
	return "\n\nConnected-account tools (MCP gateway). These reach the user's OWN " +
		"connected accounts — email, chat, issue tracker, calendar and so on — and " +
		"return their real data. " + available + "\n" +
		"- For any question about the user's own data (\"my issues\", \"my emails\", " +
		"\"messages I received\"), you MUST use these tools.\n" +
		"- NEVER answer such a question with a web search: that returns public " +
		"documentation about the product, not the user's data.\n" +
		"- After `" + searchTool + "` returns a tool, CALL THAT TOOL. Never search twice " +
		"for the same capability, and never search for a tool name you already have.\n" +
		"- If a tool reports the account is not connected or needs re-authentication, " +
		"say so plainly and stop — the platform records the request for the user to act " +
		"on. Pass on an authorization link only when the result actually contains one; " +
		"never invent or guess one."
}
