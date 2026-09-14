'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const Markdown = require('../markdown.js');

test('escapes raw HTML so model output cannot inject markup', () => {
  const out = Markdown.render('hello <script>alert(1)</script> & <b>world</b>');
  assert.equal(out.includes('<script>'), false);
  assert.equal(out.includes('&lt;script&gt;'), true);
  assert.equal(out.includes('&lt;b&gt;'), true);
  assert.equal(out.includes('&amp;'), true);
});

test('renders headings at levels 1-3', () => {
  assert.equal(Markdown.render('# One'), '<h1>One</h1>');
  assert.equal(Markdown.render('## Two'), '<h2>Two</h2>');
  assert.equal(Markdown.render('### Three'), '<h3>Three</h3>');
});

test('renders inline emphasis and code', () => {
  assert.equal(Markdown.render('**bold**'), '<p><strong>bold</strong></p>');
  assert.equal(Markdown.render('*em*'), '<p><em>em</em></p>');
  assert.equal(Markdown.render('`code`'), '<p><code>code</code></p>');
});

test('renders fenced code blocks with language', () => {
  const out = Markdown.render('```js\nvar x = 1;\n```');
  assert.equal(out.includes('<pre><code class="language-js">'), true);
  assert.equal(out.includes('var x = 1;'), true);
});

test('renders unordered and ordered lists', () => {
  assert.equal(
    Markdown.render('- a\n- b'),
    '<ul><li>a</li><li>b</li></ul>'
  );
  assert.equal(
    Markdown.render('1. a\n2. b'),
    '<ol><li>a</li><li>b</li></ol>'
  );
});

test('renders blockquotes and links', () => {
  assert.equal(Markdown.render('> quoted').includes('<blockquote>quoted</blockquote>'), true);
  assert.equal(Markdown.render('[x](https://e.com)').includes('<a href="https://e.com"'), true);
});

test('renders horizontal rule', () => {
  assert.equal(Markdown.render('---'), '<hr />');
});

test('empty input renders empty', () => {
  assert.equal(Markdown.render(''), '');
  assert.equal(Markdown.render(null), '');
});