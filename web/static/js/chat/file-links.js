(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// File links in chat messages: same-origin file links are checked, while
// cross-origin links stay as written and unprobed.
// ---------------------------------------------------------------------------
// The prefix the file server answers on: "/absolute_filepath/", the form written into chat text.
// src/runtime/file_urls.py's FILE_SERVER_MOUNTS holds that single element; the core module there
// owns the prefix. Retired "/files/" and "/file/" links are marked without probing.
var FILE_SERVER_PREFIXES = ['/absolute_filepath'];
var FILE_SERVER_PREFIX = FILE_SERVER_PREFIXES[0];
var FILE_SERVER_PREFIX_GROUP = '(?:' + FILE_SERVER_PREFIXES.join('|') + ')';
var RETIRED_FILE_SERVER_PREFIXES = ['/files/', '/file/'];

function absolutePathFromServedPathname(pathname) {
  for (var i = 0; i < FILE_SERVER_PREFIXES.length; i++) {
    var prefix = FILE_SERVER_PREFIXES[i] + '/';
    if (pathname.indexOf(prefix) !== 0) continue;
    try {
      return '/' + decodeURIComponent(pathname.slice(prefix.length));
    } catch (e) {
      console.warn('Invalid file-server path', pathname, e);
      return null;
    }
  }
  return null;
}

function parseLinkUrl(href) {
  try {
    return new URL(href, window.location.href);
  } catch (e) {
    console.warn('Invalid link', href, e);
    return null;
  }
}

// A link naming this page's hostname with a different scheme or port is pulled back to the
// page origin only when its path sits under a file-server prefix. Those are routes the page
// origin itself serves (the single prefix of FILE_SERVER_MOUNTS in
// src/runtime/file_urls.py). A link there whose scheme or port was
// written from memory is still unambiguous. Any other path names another frontend on this
// host (the publish lane and each neighboring port serve their own server), and is left
// exactly as written. Another hostname is another server, and is returned as written.
function isFileServerPath(pathname) {
  for (var i = 0; i < FILE_SERVER_PREFIXES.length; i++) {
    if (pathname === FILE_SERVER_PREFIXES[i] || pathname.indexOf(FILE_SERVER_PREFIXES[i] + '/') === 0) return true;
  }
  return false;
}

function isRetiredFileServerPath(pathname) {
  return RETIRED_FILE_SERVER_PREFIXES.some(function(prefix) { return pathname.indexOf(prefix) === 0; });
}

function resolveFileServerUrl(href) {
  return parseLinkUrl(href);
}

// ---------------------------------------------------------------------------
// Missing file links — a file link whose target the server has nothing at is
// marked where it appears. A wrong path shows up on the render rather than
// on the click that fails. A 404 marks probed links; retired prefixes are
// marked by path. Other statuses and network errors leave probed links alone.
// A link under the file-server prefix resolves to an absolute path. A package that owns a family of file links registers it:
// core then also probes a same-host link of the family that the prefix does
// not resolve. It leaves alone a served link whose answer the package's own
// fetch brings back.
// ---------------------------------------------------------------------------
var MISSING_FILE_MARKER_TEXT = '\u26A0 missing';
var MISSING_FILE_MARKER_TITLE = 'The file server found nothing at this path';
// Both recognition patterns cut a text run at the same boundaries, so the tail class is one
// definition shared by both. It ends the run at whitespace, at the closers markdown and prose
// put after a URL, and at everything outside printable ASCII. A chat URL is a maximal
// printable-ASCII run [\x21-\x7E]+, so a glued CJK character, full-width mark or emoji is
// prose, not the tail of the link. \x7F-\uFFFF also covers astral characters, whose UTF-16
// surrogate code units fall inside that range. markdown-renderer.js's tokenizer.url override
// cuts bare URLs at the same boundary — two copies of one definition, kept in sync by the
// comments in both files. The modules load independently, so sharing a constant would couple
// their load order.
var URL_TAIL_SOURCE = '[^\\s\\)\\]"\'`<>\\x7F-\\uFFFF]+';
// A run naming a path under the prefix, with or without a scheme and host in front.
var FILE_SERVER_LINK_SOURCE =
  '(?:https?:\\/\\/[^\\s]+?)?' + FILE_SERVER_PREFIX_GROUP + '\\/' + URL_TAIL_SOURCE;
var LINK_TRAILING_PUNCTUATION_RE = /[.,;:!?*，。！？；、：．]+$/;

// A family of file links that a package owns. textSource finds the family's links in message
// text (a RegExp source that ends in FILE_LINK_TAIL_SOURCE). answersFor(pathname) is true for a
// served path whose missing target the package's own fetch reports. recognizes(url) is true for
// a same-host URL of the family that the file-server prefix does not resolve.
var fileLinkFamilies = new Map();

function registerFileLinkFamily(family) {
  if (!family || typeof family.name !== 'string' || !family.name || typeof family.textSource !== 'string'
      || typeof family.answersFor !== 'function' || typeof family.recognizes !== 'function') {
    throw new Error('Invalid file link family registration');
  }
  if (fileLinkFamilies.has(family.name)) throw new Error('Duplicate file link family: ' + family.name);
  fileLinkFamilies.set(family.name, family);
}

function buildMissingMarker() {
  var marker = document.createElement('span');
  marker.className = 'file-link-missing';
  marker.title = MISSING_FILE_MARKER_TITLE;
  marker.textContent = MISSING_FILE_MARKER_TEXT;
  return marker;
}

function missingMarkerHtml() {
  return '<span class="file-link-missing" title="' + escapeHtml(MISSING_FILE_MARKER_TITLE) + '">'
    + escapeHtml(MISSING_FILE_MARKER_TEXT) + '</span>';
}

// One occurrence of a file link: the key its probe budget dedupes on. The key is the absolute
// path for a link under the prefix, and the literal URL for a family link outside it. The
// occurrence also carries the URL a probe would ask for, and where the marker goes. `end` is the
// offset just past the link inside a text node, which is where that node has to be split.
// Element carriers take the marker as a sibling instead.
function fileServerOccurrence(href, node, kind, end) {
  var url = resolveFileServerUrl(href);
  if (!url) return null;
  var pathname = url.pathname || '';
  var page = new URL(window.location.href);
  if (url.origin !== page.origin) return null;
  if (isRetiredFileServerPath(pathname)) {
    return {key: 'retired:' + url.href, probeUrl: url.href, node: node, kind: kind, end: end, immediate: true};
  }
  var absPath = absolutePathFromServedPathname(pathname);
  if (absPath !== null) {
    // A family whose own fetch answers for this path already has a request on the way; probing
    // it here would be a second request for the same thing.
    for (var family of fileLinkFamilies.values()) {
      if (family.answersFor(pathname)) return null;
    }
    return {key: absPath, probeUrl: url.href, node: node, kind: kind, end: end};
  }
  return familyOccurrence(url, node, kind, end);
}

// A link the prefix does not resolve can still name a file here when a family recognizes it; it
// is probed at its literal URL. Only links that already name the page origin reach the family
// check. A link on another host or port is left as written and unprobed because this server
// would not answer for that origin.
function familyOccurrence(url, node, kind, end) {
  var page = new URL(window.location.href);
  if (url.origin !== page.origin) return null;
  for (var family of fileLinkFamilies.values()) {
    if (family.recognizes(url)) return {key: url.href, probeUrl: url.href, node: node, kind: kind, end: end};
  }
  return null;
}

function collectFileServerOccurrences(out, text, node, kind) {
  if (!text) return;
  // One URL run can match several patterns (a scheme-full link under the prefix that a family
  // pattern also finds). The span key keeps such a run one occurrence, so neither its probe
  // budget nor its marker is doubled.
  var claimed = new Set();
  var sources = [FILE_SERVER_LINK_SOURCE, RETIRED_FILE_SERVER_LINK_SOURCE];
  fileLinkFamilies.forEach(function(family) { sources.push(family.textSource); });
  sources.forEach(function(source) {
    var pattern = new RegExp(source, 'g');
    var match;
    while ((match = pattern.exec(text)) !== null) {
      var href = match[0].replace(LINK_TRAILING_PUNCTUATION_RE, '');
      var span = match.index + ':' + (match.index + href.length);
      if (claimed.has(span)) continue;
      claimed.add(span);
      var occurrence = fileServerOccurrence(href, node, kind, match.index + href.length);
      if (occurrence) out.push(occurrence);
    }
  });
}

function probeFileMissing(probeUrl) {
  return fetch(probeUrl, {method: 'HEAD'}).then(function(resp) {
    return resp.status === 404;
  }).catch(function(e) {
    console.warn('file link probe failed', probeUrl, e);
    return false;
  });
}

function markMissingFileLinks(occurrences) {
  if (!occurrences.length) return Promise.resolve();
  var byKey = new Map();
  occurrences.forEach(function(occurrence) {
    var group = byKey.get(occurrence.key);
    if (!group) {
      group = [];
      byKey.set(occurrence.key, group);
    }
    group.push(occurrence);
  });
  var groups = Array.from(byKey.values());
  // One HEAD per unique probe target — the absolute path for links under the prefix and the
  // literal URL for family links outside it. Retired aliases are marked immediately;
  // all markers wait until probes finish so a text node splits once with every missing link.
  return Promise.all(groups.map(function(group) {
    return group[0].immediate ? Promise.resolve(true) : probeFileMissing(group[0].probeUrl);
  })).then(function(missing) {
    var marked = [];
    groups.forEach(function(group, index) {
      if (missing[index]) marked = marked.concat(group);
    });
    markOccurrencesMissing(marked);
  });
}

function markOccurrencesMissing(occurrences) {
  var textNodes = new Map();
  occurrences.forEach(function(occurrence) {
    if (occurrence.kind === 'text') {
      var group = textNodes.get(occurrence.node);
      if (!group) {
        group = [];
        textNodes.set(occurrence.node, group);
      }
      group.push(occurrence);
      return;
    }
    insertMissingMarkerAfter(occurrence.node);
  });
  textNodes.forEach(splitTextNodeAroundMarkers);
}

// The chat can be rebuilt (SPA session switch) while a probe is in flight, which leaves the
// carrier detached. A marker on a detached node would never be seen, so there is nothing to do.
function insertMissingMarkerAfter(node) {
  var parent = node.parentNode;
  if (!parent) return;
  parent.insertBefore(buildMissingMarker(), node.nextSibling);
}

// A text node carries no attribute to hang a marker off, so it is split just past each missing
// link and the marker goes between the halves. Offsets ascend, so one pass rebuilds the node.
function splitTextNodeAroundMarkers(occurrences, node) {
  var parent = node.parentNode;
  if (!parent) return;
  var text = node.nodeValue || '';
  var pieces = document.createDocumentFragment();
  var cursor = 0;
  occurrences.sort(function(a, b) { return a.end - b.end; });
  occurrences.forEach(function(occurrence) {
    pieces.appendChild(document.createTextNode(text.slice(cursor, occurrence.end)));
    pieces.appendChild(buildMissingMarker());
    cursor = occurrence.end;
  });
  pieces.appendChild(document.createTextNode(text.slice(cursor)));
  parent.replaceChild(pieces, node);
}

function localPathOccurrence(href, node) {
  if (/^[a-zA-Z][a-zA-Z0-9+.\-]*:/.test(href) || href.indexOf('/?') === 0 || href.charAt(0) === '?'
      || href.charAt(0) === '#') return null;
  var url = parseLinkUrl(href);
  if (!url) return null;
  var page = new URL(window.location.href);
  if (url.origin !== page.origin || isFileServerPath(url.pathname || '') || isRetiredFileServerPath(url.pathname || '')) {
    return null;
  }
  return {key: 'local:' + url.href, probeUrl: url.href, node: node, kind: 'element', end: null};
}

// Every message prose of a freshly rendered root: same-origin file-server links and local-path
// anchors are probed, and retired same-origin links are marked where they appear.
function markFileLinks(root) {
  root.querySelectorAll('.prose-msg').forEach(function(prose) {
    if (prose.id === 'streaming-msg' || prose.closest('#streaming-msg')) return;
    var occurrences = [];
    Array.from(prose.querySelectorAll('a[href]')).forEach(function(anchor) {
      var href = anchor.getAttribute('href') || '';
      var writtenHref = href;
      var occurrence = fileServerOccurrence(href, anchor, 'element', null);
      if (occurrence) occurrences.push(occurrence);
      else {
        var localOccurrence = localPathOccurrence(writtenHref, anchor);
        if (localOccurrence) occurrences.push(localOccurrence);
      }
    });
    Array.from(prose.querySelectorAll('code')).forEach(function(code) {
      collectFileServerOccurrences(occurrences, code.textContent || '', code, 'element');
    });
    (function scanPlainText(node) {
      if (!node) return;
      var type = node.nodeType;
      if (type === Node.TEXT_NODE) {
        collectFileServerOccurrences(occurrences, node.nodeValue || '', node, 'text');
        return;
      }
      if (type === Node.ELEMENT_NODE) {
        var tag = node.tagName;
        if (tag === 'A' || tag === 'CODE') return;
      }
      var children = node.childNodes;
      if (children) {
        for (var ci = 0; ci < children.length; ci++) scanPlainText(children[ci]);
      }
    })(prose);
    markMissingFileLinks(occurrences);
  });
}

// The tail class of a link run, for a family's textSource.
var FILE_LINK_TAIL_SOURCE = URL_TAIL_SOURCE;
var RETIRED_FILE_SERVER_LINK_SOURCE =
  'https?:\\/\\/[^\\s]+?\\/(?:files|file)\\/[^\\s\\)\\]"\'`<>\\x7F-\\uFFFF]*';

const CHAT_ONLY = {
  markFileLinks,
  registerFileLinkFamily,
  resolveFileServerUrl,
  absolutePathFromServedPathname,
  missingMarkerHtml,
  FILE_SERVER_PREFIX,
  FILE_SERVER_PREFIX_GROUP,
  FILE_LINK_TAIL_SOURCE,
};
Chat.wire({}, CHAT_ONLY);

})();
