'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const ChatState = require('../state.js');

test('visibleTurns shows user + text assistant, hides system/tool/tool-call rounds', () => {
  const thread = [
    { role: 'system', content: 'sys' },
    { role: 'user', content: 'hello' },
    { role: 'assistant', content: '', tool_calls: [{ id: 'c1', function: { name: 'bash_executor', arguments: '{}' } }] },
    { role: 'tool', tool_call_id: 'c1', content: 'result' },
    { role: 'assistant', content: 'the answer' },
    { role: 'user', content: 'thanks' },
  ];
  const turns = ChatState.visibleTurns(thread);
  assert.equal(turns.length, 3);
  assert.deepEqual(
    turns.map(t => [t.kind, t.content]),
    [['user', 'hello'], ['assistant', 'the answer'], ['user', 'thanks']]
  );
});

test('visibleTurns trims whitespace-only assistant content', () => {
  const thread = [
    { role: 'user', content: 'x' },
    { role: 'assistant', content: '\n\n' },
  ];
  assert.equal(ChatState.visibleTurns(thread).length, 1);
});

test('forkRawIndex maps a visible turn to its raw thread index', () => {
  const thread = [
    { role: 'system', content: 'sys' },           // idx 0
    { role: 'user', content: 'q1' },              // idx 1  -> visible 0
    { role: 'assistant', content: 'a1' },         // idx 2  -> visible 1
    { role: 'user', content: 'q2' },              // idx 3  -> visible 2
    { role: 'assistant', content: 'a2' },         // idx 4  -> visible 3
  ];
  assert.equal(ChatState.forkRawIndex(thread, 0), 1);
  assert.equal(ChatState.forkRawIndex(thread, 2), 3);
  assert.equal(ChatState.forkRawIndex(thread, 3), 4);
  assert.equal(ChatState.forkRawIndex(thread, 99), null);
  assert.equal(ChatState.forkRawIndex(thread, -1), null);
});

test('turnCount returns visible count', () => {
  const thread = [
    { role: 'system', content: 'sys' },
    { role: 'user', content: 'q' },
    { role: 'assistant', content: 'a' },
  ];
  assert.equal(ChatState.turnCount(thread), 2);
});

test('handles empty / non-array input', () => {
  assert.deepEqual(ChatState.visibleTurns(null), []);
  assert.deepEqual(ChatState.visibleTurns(undefined), []);
  assert.equal(ChatState.forkRawIndex([], 0), null);
});