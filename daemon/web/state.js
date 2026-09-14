// state.js — pure, DOM-free chat-session state helpers (UMD for Node tests).
//
// A session's `messages` are the OpenAI thread (system + user + assistant +
// tool). The UI shows only the "visible turns" (user text and assistant text);
// system/tool messages and assistant messages that only carry tool_calls are
// hidden. Forking maps a visible turn back to its raw thread index.

(function (root, factory) {
  if (typeof module === 'object' && module.exports) {
    module.exports = factory();
  } else {
    root.ChatState = factory();
  }
}(typeof self !== 'undefined' ? self : this, function () {
  'use strict';

  function contentOf(m) {
    return (m && m.content && typeof m.content === 'string') ? m.content.trim() : '';
  }

  function hasToolCalls(m) {
    return !!(m && m.tool_calls && m.tool_calls.length);
  }

  // Visible turns: [{ kind: 'user'|'assistant', content, rawIndex }].
  function visibleTurns(messages) {
    var turns = [];
    if (!Array.isArray(messages)) return turns;
    for (var i = 0; i < messages.length; i++) {
      var m = messages[i];
      var role = m && m.role;
      if (role === 'user') {
        turns.push({ kind: 'user', content: contentOf(m), rawIndex: i });
      } else if (role === 'assistant') {
        // Hide pure tool-call rounds (no visible text).
        var text = contentOf(m);
        if (text) turns.push({ kind: 'assistant', content: text, rawIndex: i });
      }
      // system and tool messages are never shown.
    }
    return turns;
  }

  // Map a visible turn index to the raw thread index to fork at (truncate the
  // thread up to and including that message). Returns null when out of range.
  function forkRawIndex(messages, visibleIndex) {
    var turns = visibleTurns(messages);
    if (visibleIndex < 0 || visibleIndex >= turns.length) return null;
    return turns[visibleIndex].rawIndex;
  }

  // Number of visible turns, for UI clamping.
  function turnCount(messages) {
    return visibleTurns(messages).length;
  }

  return {
    visibleTurns: visibleTurns,
    forkRawIndex: forkRawIndex,
    turnCount: turnCount
  };
}));