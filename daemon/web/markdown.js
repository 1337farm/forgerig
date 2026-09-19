// markdown.js — minimal, dependency-free Markdown → HTML renderer.
//
// Pure (string in, string out) so it runs in the browser AND under Node for
// unit tests (UMD). Raw HTML in the source is escaped first, so model output
// is rendered as code/text and can never inject into the page.

(function (root, factory) {
  if (typeof module === 'object' && module.exports) {
    module.exports = factory();
  } else {
    root.Markdown = factory();
  }
}(typeof self !== 'undefined' ? self : this, function () {
  'use strict';

  function escapeHtml(s) {
    return s
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function codeBlock(code, lang) {
    var cls = lang ? ' class="language-' + escapeHtml(lang) + '"' : '';
    return '<pre><code' + cls + '>' + escapeHtml(code).replace(/\n$/, '') + '</code></pre>';
  }

  // Inline formatting: escape first, then code spans act as protected islands,
  // then bold/italic/links/underline on the remaining text.
  function inline(src) {
    var s = escapeHtml(src);
    s = s.replace(/`([^`\n]+)`/g, '<code>$1</code>');
    s = s.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
    s = s.replace(/__([^_\n]+)__/g, '<strong>$1</strong>');
    s = s.replace(/(^|[^*\w])\*([^*\n]+)\*(?!\*)/g, '$1<em>$2</em>');
    s = s.replace(/(^|[^_\w])_([^_\n]+)_(?!_)/g, '$1<em>$2</em>');
    s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
    return s;
  }

  function render(src) {
    if (src == null) return '';
    var text = String(src).replace(/\r\n/g, '\n').replace(/\r/g, '\n');
    var lines = text.split('\n');
    var html = [];
    var i = 0;

    function renderList(items, ordered) {
      var tag = ordered ? 'ol' : 'ul';
      var lis = items.map(function (it) { return '<li>' + inline(it) + '</li>'; }).join('');
      return '<' + tag + '>' + lis + '</' + tag + '>';
    }

    function splitRow(line) {
      var cells = line.trim().split('|');
      if (cells.length && /^\s*$/.test(cells[0])) cells.shift();
      if (cells.length && /^\s*$/.test(cells[cells.length - 1])) cells.pop();
      return cells.map(function (c) { return c.trim(); });
    }

    function isDelimRow(line) {
      var cells = splitRow(line);
      if (!cells.length) return false;
      return cells.every(function (c) { return /^:?-+:?$/.test(c); });
    }

    function isTableStart(idx) {
      return idx + 1 < lines.length
        && lines[idx].indexOf('|') !== -1
        && isDelimRow(lines[idx + 1]);
    }

    function renderTable(header, aligns, rows) {
      var thead = '<thead><tr>' + header.map(function (h, c) {
        var a = aligns[c] ? ' align="' + aligns[c] + '"' : '';
        return '<th' + a + '>' + inline(h) + '</th>';
      }).join('') + '</tr></thead>';
      var tbody = '<tbody>' + rows.map(function (r) {
        return '<tr>' + r.map(function (cell, c) {
          var a = aligns[c] ? ' align="' + aligns[c] + '"' : '';
          return '<td' + a + '>' + inline(cell || '') + '</td>';
        }).join('') + '</tr>';
      }).join('') + '</tbody>';
      return '<table>' + thead + tbody + '</table>';
    }

    while (i < lines.length) {
      var line = lines[i];

      // Blank line -> paragraph break / list reset.
      if (/^\s*$/.test(line)) { i++; continue; }

      // Fenced code block.
      var fence = /^```(.*)$/.exec(line);
      if (fence) {
        var lang = fence[1].trim();
        var code = [];
        i++;
        while (i < lines.length && !/^```\s*$/.test(lines[i])) {
          code.push(lines[i]);
          i++;
        }
        if (i < lines.length) i++; // skip closing fence
        html.push(codeBlock(code.join('\n'), lang));
        continue;
      }

      // Heading.
      var heading = /^(#{1,6})\s+(.*)$/.exec(line);
      if (heading) {
        var level = heading[1].length;
        html.push('<h' + level + '>' + inline(heading[2]) + '</h' + level + '>');
        i++;
        continue;
      }

      // Horizontal rule.
      if (/^\s*(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) { html.push('<hr />'); i++; continue; }

      // Blockquote (consecutive ">" lines).
      if (/^\s*>\s?/.test(line)) {
        var quote = [];
        while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
          quote.push(lines[i].replace(/^\s*>\s?/, ''));
          i++;
        }
        html.push('<blockquote>' + quote.map(inline).join('<br />') + '</blockquote>');
        continue;
      }

      // Unordered list.
      var ulMatch = /^\s*[-*+]\s+(.*)$/.exec(line);
      if (ulMatch) {
        var items = [];
        while (i < lines.length) {
          var m = /^\s*[-*+]\s+(.*)$/.exec(lines[i]);
          if (!m) break;
          items.push(m[1]);
          i++;
        }
        html.push(renderList(items, false));
        continue;
      }

      // Ordered list.
      var olMatch = /^\s*\d+[.)]\s+(.*)$/.exec(line);
      if (olMatch) {
        var oitems = [];
        while (i < lines.length) {
          var m2 = /^\s*\d+[.)]\s+(.*)$/.exec(lines[i]);
          if (!m2) break;
          oitems.push(m2[1]);
          i++;
        }
        html.push(renderList(oitems, true));
        continue;
      }

      // GFM table: header row + delimiter row, then body rows.
      if (isTableStart(i)) {
        var header = splitRow(line);
        var aligns = splitRow(lines[i + 1]).map(function (d) {
          var left = d.charAt(0) === ':';
          var right = d.charAt(d.length - 1) === ':';
          return left && right ? 'center' : (right ? 'right' : (left ? 'left' : ''));
        });
        i += 2;
        var rows = [];
        while (i < lines.length && !/^\s*$/.test(lines[i]) && lines[i].indexOf('|') !== -1) {
          rows.push(splitRow(lines[i]));
          i++;
        }
        html.push(renderTable(header, aligns, rows));
        continue;
      }

      // Paragraph: gather consecutive non-blank, non-special lines.
      var para = [line];
      i++;
      while (i < lines.length && !/^\s*$/.test(lines[i])) {
        // stop at block-level markers
        if (/^```/.test(lines[i])) break;
        if (/^(#{1,6})\s+/.test(lines[i])) break;
        if (/^\s*[-*+]\s+/.test(lines[i])) break;
        if (/^\s*\d+[.)]\s+/.test(lines[i])) break;
        if (/^\s*>\s?/.test(lines[i])) break;
        if (isTableStart(i)) break;
        para.push(lines[i]);
        i++;
      }
      html.push('<p>' + para.map(inline).join('<br />') + '</p>');
    }

    return html.join('\n');
  }

  return { render: render };
}));