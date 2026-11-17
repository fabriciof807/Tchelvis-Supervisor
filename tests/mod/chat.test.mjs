import { test } from "node:test";
import assert from "node:assert/strict";
import {
  chatHistoryForRequest,
  chatRequestArgv,
  parseChatResult,
  chatHeaderLabel,
  fallbackMarker,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("history keeps the last 12 user/assistant messages and skips errors", () => {
  const msgs = [];
  for (let i = 0; i < 15; i += 1) {
    if (i === 3 || i === 11) {
      msgs.push({ role: "error", content: `err-${i}` });
    }
    msgs.push({
      role: i % 2 === 0 ? "user" : "assistant",
      content: `m-${i}`,
      extra: "ignored",
    });
  }
  const res = chatHistoryForRequest(msgs);
  assert.equal(res.length, 12);
  assert.ok(res.every((m) => m.role === "user" || m.role === "assistant"));
  assert.deepEqual(Object.keys(res[0]).sort(), ["content", "role"]);
  assert.equal(res[0].content, "m-3");
  assert.equal(res[11].content, "m-14");
});

test("request argv carries question and history as JSON", () => {
  const argv = chatRequestArgv(
    [
      { role: "user", content: "hi" },
      { role: "error", content: "oops" },
    ],
    "q"
  );
  assert.equal(argv[0], "chat");
  assert.equal(argv[1], "ask");
  assert.equal(argv[2], "--json");
  assert.equal(argv[3], "--request");
  const parsed = JSON.parse(argv[4]);
  assert.equal(parsed.question, "q");
  assert.deepEqual(parsed.history, [{ role: "user", content: "hi" }]);
});

test("parseChatResult maps answer, llm_stopped, llm_busy and garbage", () => {
  assert.deepEqual(
    parseChatResult(
      0,
      JSON.stringify({
        answer: "tudo ok",
        model: "LFM3-1.7B",
        via_fallback: false,
        fallback_host: null,
        context_chars: 120,
      })
    ),
    {
      ok: true,
      answer: "tudo ok",
      model: "LFM3-1.7B",
      viaFallback: false,
      fallbackHost: null,
    }
  );

  assert.deepEqual(
    parseChatResult(
      1,
      JSON.stringify({ code: "llm_stopped", error: "LLM desligada: use Load LLM" })
    ),
    {
      ok: false,
      code: "llm_stopped",
      error: "LLM desligada: use Load LLM",
    }
  );

  assert.deepEqual(
    parseChatResult(
      1,
      JSON.stringify({ code: "llm_busy", error: "Supervisor avaliando, tente de novo" })
    ),
    {
      ok: false,
      code: "llm_busy",
      error: "Supervisor avaliando, tente de novo",
    }
  );

  assert.deepEqual(parseChatResult(1, "not json"), {
    ok: false,
    code: "llm_error",
    error: "Resposta inválida do tchelvis",
  });
});

test("header shows the fallback hostname only when active", () => {
  assert.equal(
    chatHeaderLabel({
      llm_model: "LFM3-1.7B",
      fallback_active: true,
      fallback_host: "https://openrouter.ai/api/v1",
    }),
    "LFM3-1.7B (Fallback · openrouter.ai)"
  );
  assert.equal(
    chatHeaderLabel({
      llm_model: "LFM3-1.7B",
      fallback_active: false,
      fallback_host: "https://openrouter.ai/api/v1",
    }),
    "LFM3-1.7B"
  );
});

test("fallback marker only on answers that came via fallback", () => {
  assert.equal(
    fallbackMarker({ viaFallback: true, fallbackHost: "https://openrouter.ai/api/v1" }),
    "via Fallback · openrouter.ai"
  );
  assert.equal(
    fallbackMarker({ viaFallback: false, fallbackHost: "https://openrouter.ai/api/v1" }),
    ""
  );
});
