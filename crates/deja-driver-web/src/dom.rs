//! Single-eval Shadow-DOM-aware accessibility/DOM hierarchy snapshot script,
//! Rust `UiHierarchy` conversion with `known_hash` short-circuiting, and pure-Rust
//! RFC 4648 base64 PNG decoding.

use deja_flow::{ObservationSource, UiHierarchy, UiNode, UiRect, UiSize};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{Result, WebDriverError};

/// Maximum number of DOM nodes emitted per hierarchy snapshot.
pub const MAX_DOM_NODES: usize = 1000;

/// Single-evaluation JavaScript DOM/accessibility tree walker executed via CDP `Runtime.evaluate`.
///
/// Design invariants:
/// - DFS pre-order traversal with open Shadow DOM awareness without `<slot>` duplication.
/// - Caps output at `1000` nodes (`truncated: true` if any visible/meaningful nodes remain).
/// - Prunes `[hidden]`, `aria-hidden="true"`, `inert`, and `display: none` subtrees early.
/// - Computes viewport + overflow-ancestor clipped visibility (`isOnScreen`) while retaining
///   off-screen nodes with their real bounding rect so `scrollUntilVisible` works accurately.
/// - Computes `hittable` via `isOnScreen && enabled && pointerEvents !== 'none'` plus deep
///   shadow-piercing `elementFromPoint` occlusion checking for interactive elements.
/// - Avoids subtree text concatenation bloat on layout containers.
pub const AX_DOM_SCRIPT: &str = r#"(() => {
  const MAX_NODES = 1000;
  const SKIP_TAGS = new Set([
    'SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'META', 'LINK', 'TITLE', 'HEAD', 'BASE', 'BR', 'WBR'
  ]);
  const INTERACTIVE_TAGS = new Set([
    'A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'SUMMARY', 'DETAILS', 'OPTION'
  ]);
  const INTERACTIVE_ROLES = new Set([
    'button', 'link', 'checkbox', 'radio', 'switch', 'tab', 'menuitem',
    'menuitemcheckbox', 'menuitemradio', 'textbox', 'combobox', 'listbox',
    'option', 'slider', 'spinbutton', 'searchbox', 'treeitem'
  ]);
  const CONTAINER_TAGS = new Set([
    'BODY', 'MAIN', 'SECTION', 'ARTICLE', 'NAV', 'ASIDE', 'HEADER', 'FOOTER',
    'DIV', 'FORM', 'UL', 'OL', 'DL', 'TABLE', 'THEAD', 'TBODY', 'TFOOT', 'TR',
    'FIELDSET', 'DIALOG'
  ]);
  const LANDMARK_OR_SEMANTIC_TAGS = new Set([
    'MAIN', 'SECTION', 'ARTICLE', 'NAV', 'ASIDE', 'HEADER', 'FOOTER', 'FORM',
    'DIALOG', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'P', 'LABEL', 'IMG', 'LI',
    'UL', 'OL', 'TABLE', 'TR', 'TD', 'TH', 'CAPTION', 'LEGEND', 'FIGCAPTION',
    'DT', 'DD', 'SPAN', 'STRONG', 'EM', 'CODE', 'PRE', 'BLOCKQUOTE'
  ]);

  const vw = window.innerWidth || document.documentElement.clientWidth || 0;
  const vh = window.innerHeight || document.documentElement.clientHeight || 0;
  const vpClip = { left: 0, top: 0, right: vw, bottom: vh };

  function cleanText(s) {
    if (!s || typeof s !== 'string') return '';
    return s.replace(/\s+/g, ' ').trim();
  }

  function deepActiveElement() {
    let cur = document.activeElement;
    let guard = 0;
    while (cur && cur.shadowRoot && cur.shadowRoot.activeElement && guard < 32) {
      cur = cur.shadowRoot.activeElement;
      guard++;
    }
    return cur;
  }

  function deepElementFromPoint(x, y) {
    if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
    if (x < 0 || y < 0 || x >= vw || y >= vh) return null;
    let hit = document.elementFromPoint(x, y);
    let guard = 0;
    while (hit && hit.shadowRoot && typeof hit.shadowRoot.elementFromPoint === 'function' && guard < 32) {
      const inner = hit.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === hit) break;
      hit = inner;
      guard++;
    }
    return hit;
  }

  function isContainedComposed(container, target) {
    if (!container || !target) return false;
    if (container === target) return true;
    if (typeof container.contains === 'function' && container.contains(target)) return true;
    let cur = target;
    let guard = 0;
    while (cur && guard < 64) {
      if (cur === container) return true;
      if (cur.assignedSlot) {
        cur = cur.assignedSlot;
      } else if (cur.parentNode) {
        cur = cur.parentNode;
      } else if (cur.host) {
        cur = cur.host;
      } else {
        break;
      }
      guard++;
    }
    return false;
  }

  function directText(el) {
    let out = '';
    for (let child = el.firstChild; child; child = child.nextSibling) {
      if (child.nodeType === Node.TEXT_NODE && child.nodeValue) {
        out += ' ' + child.nodeValue;
      }
    }
    return cleanText(out);
  }

  function hasBlockOrInteractiveChild(el) {
    for (let i = 0; i < el.children.length; i++) {
      const c = el.children[i];
      const ct = c.tagName ? c.tagName.toUpperCase() : '';
      if (CONTAINER_TAGS.has(ct) || INTERACTIVE_TAGS.has(ct)) return true;
      const cr = (c.getAttribute('role') || '').trim().toLowerCase();
      if (INTERACTIVE_ROLES.has(cr)) return true;
    }
    return false;
  }

  function visibleSubtreeText(el) {
    let out = '';
    function collect(node) {
      if (node.nodeType === Node.TEXT_NODE) {
        if (node.nodeValue) out += ' ' + node.nodeValue;
        return;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) return;
      const tag = node.tagName ? node.tagName.toUpperCase() : '';
      if (SKIP_TAGS.has(tag)) return;
      if (node.hasAttribute('hidden') || node.getAttribute('aria-hidden') === 'true' || node.inert) return;
      const st = window.getComputedStyle(node);
      const op = parseFloat(st.opacity);
      if (
        st.display === 'none' ||
        st.visibility === 'hidden' ||
        st.visibility === 'collapse' ||
        (Number.isFinite(op) && op <= 0)
      ) {
        return;
      }
      if (node.shadowRoot) {
        for (let i = 0; i < node.shadowRoot.childNodes.length; i++) {
          collect(node.shadowRoot.childNodes[i]);
        }
        return;
      }
      if (tag === 'SLOT' && typeof node.assignedNodes === 'function') {
        const assigned = node.assignedNodes({ flatten: true });
        if (assigned.length > 0) {
          for (let i = 0; i < assigned.length; i++) collect(assigned[i]);
          return;
        }
      }
      for (let i = 0; i < node.childNodes.length; i++) {
        collect(node.childNodes[i]);
      }
    }
    collect(el);
    return cleanText(out);
  }

  function resolveAriaLabelledBy(el) {
    const raw = el.getAttribute('aria-labelledby');
    if (!raw) return '';
    const root = typeof el.getRootNode === 'function' ? el.getRootNode() : document;
    const parts = [];
    for (const id of raw.trim().split(/\s+/)) {
      if (!id) continue;
      let ref = (root && typeof root.getElementById === 'function') ? root.getElementById(id) : null;
      if (!ref) ref = document.getElementById(id);
      if (ref) {
        const t = visibleSubtreeText(ref) || cleanText(ref.textContent);
        if (t) parts.push(t);
      }
    }
    return cleanText(parts.join(' '));
  }

  function resolveFormLabel(el) {
    if (el.labels && el.labels.length > 0) {
      const parts = [];
      for (let i = 0; i < el.labels.length; i++) {
        const t = visibleSubtreeText(el.labels[i]);
        if (t) parts.push(t);
      }
      const joined = cleanText(parts.join(' '));
      if (joined) return joined;
    }
    if (el.id) {
      const root = typeof el.getRootNode === 'function' ? el.getRootNode() : document;
      const scope = (root && typeof root.querySelectorAll === 'function') ? root : document;
      const labels = scope.querySelectorAll('label[for]');
      for (let i = 0; i < labels.length; i++) {
        if (labels[i].getAttribute('for') === el.id) {
          const t = visibleSubtreeText(labels[i]);
          if (t) return t;
        }
      }
    }
    const enclosing = typeof el.closest === 'function' ? el.closest('label') : null;
    if (enclosing && enclosing !== el) {
      const t = directText(enclosing);
      if (t) return t;
    }
    return '';
  }

  function normalizeElementType(el, tag, role) {
    if (role) {
      switch (role) {
        case 'button': return 'Button';
        case 'link': return 'Link';
        case 'textbox': return 'TextField';
        case 'searchbox': return 'SearchField';
        case 'checkbox': return 'CheckBox';
        case 'radio': return 'RadioButton';
        case 'switch': return 'Switch';
        case 'combobox': return 'ComboBox';
        case 'listbox': return 'ListBox';
        case 'option': return 'Option';
        case 'tab': return 'Tab';
        case 'tabpanel': return 'TabPanel';
        case 'heading': return 'Heading';
        case 'img': return 'Image';
        case 'dialog': case 'alertdialog': return 'Dialog';
        case 'alert': return 'Alert';
        case 'status': return 'Status';
        case 'navigation': return 'Navigation';
        case 'main': return 'Main';
        case 'region': return 'Region';
        case 'list': return 'List';
        case 'listitem': return 'ListItem';
        case 'form': return 'Form';
        case 'slider': return 'Slider';
        case 'menuitem': case 'menuitemcheckbox': case 'menuitemradio': return 'MenuItem';
        case 'group': return 'Group';
        default: break;
      }
    }
    switch (tag) {
      case 'BUTTON': return 'Button';
      case 'A': return 'Link';
      case 'INPUT': {
        const it = (el.getAttribute('type') || 'text').toLowerCase();
        if (it === 'password') return 'SecureTextField';
        if (it === 'checkbox') return 'CheckBox';
        if (it === 'radio') return 'RadioButton';
        if (it === 'button' || it === 'submit' || it === 'reset') return 'Button';
        if (it === 'search') return 'SearchField';
        if (it === 'range') return 'Slider';
        return 'TextField';
      }
      case 'TEXTAREA': return 'TextArea';
      case 'SELECT': return 'ComboBox';
      case 'OPTION': return 'Option';
      case 'H1': case 'H2': case 'H3': case 'H4': case 'H5': case 'H6': return 'Heading';
      case 'P': case 'SPAN': case 'LABEL': case 'STRONG': case 'EM': case 'CODE':
      case 'PRE': case 'BLOCKQUOTE': case 'LEGEND': case 'FIGCAPTION': case 'CAPTION':
      case 'DT': case 'DD':
        return 'StaticText';
      case 'IMG': case 'SVG': return 'Image';
      case 'DIALOG': return 'Dialog';
      case 'NAV': return 'Navigation';
      case 'MAIN': return 'Main';
      case 'SECTION': case 'ARTICLE': case 'ASIDE': case 'HEADER': case 'FOOTER': return 'Region';
      case 'UL': case 'OL': case 'DL': return 'List';
      case 'LI': return 'ListItem';
      case 'FORM': return 'Form';
      case 'SUMMARY': return 'Button';
      case 'BODY': return 'Application';
      default: return 'Group';
    }
  }

  function isInteractive(el, tag, role) {
    if (INTERACTIVE_TAGS.has(tag)) {
      if (tag === 'A') return el.hasAttribute('href') || role === 'link' || role === 'button';
      return true;
    }
    if (role && INTERACTIVE_ROLES.has(role)) return true;
    if (el.isContentEditable) return true;
    const ti = el.getAttribute('tabindex');
    if (ti !== null && ti !== '' && Number(ti) >= 0) return true;
    return false;
  }

  function extractValue(el, tag, role) {
    if (tag === 'INPUT') {
      const it = (el.getAttribute('type') || 'text').toLowerCase();
      if (it === 'checkbox' || it === 'radio' || it === 'button' || it === 'submit' || it === 'reset') {
        return null;
      }
      const v = typeof el.value === 'string' ? el.value : '';
      if (!v) return null;
      if (it === 'password') return '\u2022'.repeat(v.length);
      return v;
    }
    if (tag === 'TEXTAREA') {
      const v = typeof el.value === 'string' ? el.value : '';
      return v.length > 0 ? v : null;
    }
    if (tag === 'SELECT') {
      if (el.selectedOptions && el.selectedOptions.length > 0) {
        const txt = cleanText(el.selectedOptions[0].textContent);
        if (txt) return txt;
      }
      const v = typeof el.value === 'string' ? el.value : '';
      return v.length > 0 ? v : null;
    }
    if (el.isContentEditable || role === 'textbox') {
      const t = visibleSubtreeText(el);
      return t.length > 0 ? t : null;
    }
    const vt = el.getAttribute('aria-valuetext');
    if (vt && vt.trim()) return vt.trim();
    const vn = el.getAttribute('aria-valuenow');
    if (vn && vn.trim()) return vn.trim();
    return null;
  }

  function extractAccessibleName(el, tag) {
    const labelledBy = resolveAriaLabelledBy(el);
    if (labelledBy) return labelledBy;
    const ariaLabel = cleanText(el.getAttribute('aria-label'));
    if (ariaLabel) return ariaLabel;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') {
      const formLabel = resolveFormLabel(el);
      if (formLabel) return formLabel;
      const it = (el.getAttribute('type') || '').toLowerCase();
      if (it === 'image') {
        const alt = cleanText(el.getAttribute('alt'));
        if (alt) return alt;
      }
      return '';
    }
    if (tag === 'IMG' || tag === 'AREA') {
      return cleanText(el.getAttribute('alt'));
    }
    return '';
  }

  function extractVisibleText(el, tag, role) {
    if (tag === 'INPUT') {
      const it = (el.getAttribute('type') || '').toLowerCase();
      if (it === 'button' || it === 'submit' || it === 'reset') {
        return cleanText(el.value);
      }
      return '';
    }
    if (tag === 'TEXTAREA' || tag === 'SELECT' || tag === 'IMG' || tag === 'AREA') {
      return '';
    }
    if (
      tag === 'BUTTON' || tag === 'A' || tag === 'SUMMARY' || tag === 'OPTION' ||
      tag === 'LABEL' || tag === 'P' || /^H[1-6]$/.test(tag) ||
      role === 'button' || role === 'link' || role === 'alert' || role === 'status' ||
      role === 'tab' || role === 'menuitem' || role === 'heading'
    ) {
      return visibleSubtreeText(el);
    }
    if (CONTAINER_TAGS.has(tag)) {
      return directText(el);
    }
    if (!hasBlockOrInteractiveChild(el)) {
      return visibleSubtreeText(el);
    }
    return directText(el);
  }

  function intersectClip(clip, rect, clipX, clipY) {
    return {
      left: clipX ? Math.max(clip.left, rect.left) : clip.left,
      top: clipY ? Math.max(clip.top, rect.top) : clip.top,
      right: clipX ? Math.min(clip.right, rect.right) : clip.right,
      bottom: clipY ? Math.min(clip.bottom, rect.bottom) : clip.bottom,
    };
  }

  const activeEl = deepActiveElement();
  const seen = new WeakSet();
  const nodes = [];
  let truncated = false;

  function childElementsOf(el) {
    const tag = el.tagName ? el.tagName.toUpperCase() : '';
    if (el.shadowRoot) {
      return Array.from(el.shadowRoot.children);
    }
    if (tag === 'SLOT' && typeof el.assignedElements === 'function') {
      const assigned = el.assignedElements({ flatten: true });
      if (assigned.length > 0) return assigned;
      return Array.from(el.children);
    }
    return Array.from(el.children);
  }

  function walk(el, parentIdx, depth, inheritedClip, ancestorOpaque) {
    if (!el || el.nodeType !== Node.ELEMENT_NODE) return;
    if (seen.has(el)) return;
    seen.add(el);

    const tag = el.tagName ? el.tagName.toUpperCase() : '';
    if (SKIP_TAGS.has(tag)) return;

    // Early subtree pruning for hidden / aria-hidden / inert / display:none.
    if (el.hasAttribute('hidden') || el.getAttribute('aria-hidden') === 'true' || el.inert) {
      return;
    }
    const style = window.getComputedStyle(el);
    if (style.display === 'none') {
      return;
    }

    const rawOpacity = parseFloat(style.opacity);
    const selfOpaque = !(Number.isFinite(rawOpacity) && rawOpacity <= 0);
    const effectiveOpaque = ancestorOpaque && selfOpaque;

    const effectiveClip = style.position === 'fixed' ? vpClip : inheritedClip;
    const rect = el.getBoundingClientRect();
    const role = (el.getAttribute('role') || '').trim().toLowerCase();
    const interactive = isInteractive(el, tag, role);
    const isSlot = tag === 'SLOT';

    const testId = cleanText(el.getAttribute('data-testid'));
    const domId = cleanText(el.id);
    const nameAttr = cleanText(el.getAttribute('name'));
    const identifier = testId || domId || nameAttr || '';
    const titleAttr = cleanText(el.getAttribute('title'));
    const placeholderRaw = el.getAttribute('placeholder');
    const placeholderValue = placeholderRaw && placeholderRaw.trim() ? placeholderRaw.trim() : null;
    const value = extractValue(el, tag, role);
    const accessibleName = isSlot ? '' : extractAccessibleName(el, tag);
    const visibleText = isSlot ? '' : extractVisibleText(el, tag, role);
    const label = accessibleName || visibleText;
    const title = (accessibleName && visibleText && visibleText !== accessibleName)
      ? visibleText
      : titleAttr;

    const hasMeaning = !isSlot && (
      tag === 'BODY' ||
      interactive ||
      identifier !== '' ||
      label !== '' ||
      title !== '' ||
      value !== null ||
      placeholderValue !== null ||
      role !== '' ||
      (LANDMARK_OR_SEMANTIC_TAGS.has(tag) && (rect.width > 0 || rect.height > 0))
    );

    let nextParent = parentIdx;
    let nextDepth = depth;

    if (hasMeaning) {
      if (nodes.length >= MAX_NODES) {
        truncated = true;
        return;
      }

      const visLeft = Math.max(rect.left, effectiveClip.left);
      const visTop = Math.max(rect.top, effectiveClip.top);
      const visRight = Math.min(rect.right, effectiveClip.right);
      const visBottom = Math.min(rect.bottom, effectiveClip.bottom);
      const visW = Math.max(0, visRight - visLeft);
      const visH = Math.max(0, visBottom - visTop);
      const visuallyHidden =
        !effectiveOpaque ||
        style.visibility === 'hidden' ||
        style.visibility === 'collapse';
      const isOnScreen =
        !visuallyHidden &&
        rect.width > 0 &&
        rect.height > 0 &&
        visW > 0 &&
        visH > 0;

      let nativelyDisabled = false;
      try {
        nativelyDisabled = typeof el.matches === 'function' && el.matches(':disabled');
      } catch (_err) {
        nativelyDisabled = Boolean(el.disabled);
      }
      const enabled =
        !nativelyDisabled &&
        el.getAttribute('aria-disabled') !== 'true';
      const pointerEventsNone = style.pointerEvents === 'none';

      let hittable = isOnScreen && enabled && !pointerEventsNone;
      if (hittable && interactive) {
        const cx = visLeft + visW / 2;
        const cy = visTop + visH / 2;
        const hit = deepElementFromPoint(cx, cy);
        if (hit && !isContainedComposed(el, hit)) {
          hittable = false;
        }
      }

      const checked =
        Boolean(el.checked) || el.getAttribute('aria-checked') === 'true';
      const selected =
        Boolean(el.selected) ||
        el.getAttribute('aria-selected') === 'true' ||
        el.getAttribute('aria-current') === 'true' ||
        el.getAttribute('aria-current') === 'page';
      const hasFocus = el === activeEl;

      const idx = nodes.length;
      nodes.push({
        index: idx,
        parentIndex: parentIdx,
        depth: depth,
        elementType: normalizeElementType(el, tag, role),
        identifier: identifier,
        label: label,
        title: title,
        value: value,
        placeholderValue: placeholderValue,
        enabled: enabled,
        checked: checked,
        selected: selected,
        hasFocus: hasFocus,
        hittable: hittable,
        isOnScreen: isOnScreen,
        frame: {
          x: Number.isFinite(rect.left) ? rect.left : 0,
          y: Number.isFinite(rect.top) ? rect.top : 0,
          width: Number.isFinite(rect.width) ? rect.width : 0,
          height: Number.isFinite(rect.height) ? rect.height : 0,
        },
        visibleFrame: isOnScreen
          ? {
              x: Number.isFinite(visLeft) ? visLeft : 0,
              y: Number.isFinite(visTop) ? visTop : 0,
              width: Number.isFinite(visW) ? visW : 0,
              height: Number.isFinite(visH) ? visH : 0,
            }
          : null,
      });
      nextParent = idx;
      nextDepth = depth + 1;
    }

    const ox = style.overflowX || '';
    const oy = style.overflowY || '';
    const clipX = ox === 'hidden' || ox === 'clip' || ox === 'scroll' || ox === 'auto';
    const clipY = oy === 'hidden' || oy === 'clip' || oy === 'scroll' || oy === 'auto';
    let childClip = effectiveClip;
    if (clipX || clipY) {
      const borderLeft = parseFloat(style.borderLeftWidth || '0') || 0;
      const borderTop = parseFloat(style.borderTopWidth || '0') || 0;
      const scrollportLeft = rect.left + borderLeft;
      const scrollportTop = rect.top + borderTop;
      const scrollportRect = {
        left: scrollportLeft,
        top: scrollportTop,
        right: scrollportLeft + (el.clientWidth || 0),
        bottom: scrollportTop + (el.clientHeight || 0),
      };
      childClip = intersectClip(effectiveClip, scrollportRect, clipX, clipY);
    }

    const children = childElementsOf(el);
    for (let i = 0; i < children.length; i++) {
      if (truncated) return;
      walk(children[i], nextParent, nextDepth, childClip, effectiveOpaque);
    }
  }

  const root = document.body || document.documentElement;
  if (root) {
    walk(root, null, 0, vpClip, true);
  }

  return {
    url: String(window.location ? window.location.href : ''),
    viewport: { width: vw, height: vh },
    truncated: truncated,
    nodes: nodes,
  };
})()"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDomSnapshot {
    #[serde(default)]
    viewport: RawViewport,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    nodes: Vec<RawDomNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawViewport {
    #[serde(default)]
    width: f64,
    #[serde(default)]
    height: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDomNode {
    index: usize,
    #[serde(default)]
    parent_index: Option<usize>,
    #[serde(default)]
    depth: usize,
    #[serde(default)]
    element_type: String,
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    placeholder_value: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    checked: bool,
    #[serde(default)]
    selected: bool,
    #[serde(default)]
    has_focus: bool,
    #[serde(default)]
    hittable: bool,
    #[serde(default)]
    is_on_screen: bool,
    #[serde(default)]
    frame: RawRect,
    #[serde(default)]
    visible_frame: Option<RawRect>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRect {
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
    #[serde(default)]
    width: f64,
    #[serde(default)]
    height: f64,
}

fn default_true() -> bool {
    true
}

/// Converts the JSON payload returned by [`AX_DOM_SCRIPT`] into a [`UiHierarchy`], computes the
/// deterministic FNV-1a hierarchy hash via `deja_flow`, and applies the `known_hash` short-circuit
/// (returning `unchanged: true` with empty `nodes` when `!truncated` and hash matches).
pub(crate) fn dom_snapshot_to_hierarchy(
    raw: Value,
    app_id: &str,
    known_hash: Option<&str>,
) -> Result<UiHierarchy> {
    let snap: RawDomSnapshot =
        serde_json::from_value(raw).map_err(|source| WebDriverError::Json {
            context: "decode AX-DOM snapshot".to_string(),
            source,
        })?;

    let truncated = snap.truncated || snap.nodes.len() > MAX_DOM_NODES;
    let nodes: Vec<UiNode> = snap
        .nodes
        .into_iter()
        .take(MAX_DOM_NODES)
        .map(|n| UiNode {
            index: n.index,
            parent_index: n.parent_index,
            depth: n.depth,
            source: "web".to_string(),
            observation_source: ObservationSource::Tree,
            element_type: n.element_type,
            identifier: n.identifier,
            label: n.label,
            title: n.title,
            value: n.value,
            placeholder_value: n.placeholder_value,
            enabled: n.enabled,
            checked: n.checked,
            selected: n.selected,
            has_focus: n.has_focus,
            hittable: n.hittable,
            is_on_screen: n.is_on_screen,
            frame: UiRect::new(n.frame.x, n.frame.y, n.frame.width, n.frame.height),
            visible_frame: n
                .visible_frame
                .map(|vf| UiRect::new(vf.x, vf.y, vf.width, vf.height))
                .filter(|r| !r.is_empty()),
        })
        .collect();

    let viewport = UiSize::new(snap.viewport.width, snap.viewport.height);
    let mut hierarchy = UiHierarchy {
        app_id: app_id.to_string(),
        state: "running".to_string(),
        viewport: viewport.is_positive().then_some(viewport),
        truncated,
        hierarchy_hash: String::new(),
        unchanged: false,
        nodes,
    };

    let computed_hash = hierarchy.hierarchy_hash_hex();
    hierarchy.hierarchy_hash = computed_hash.clone();

    if !hierarchy.truncated
        && let Some(expected) = known_hash
        && expected.eq_ignore_ascii_case(&computed_hash)
    {
        hierarchy.unchanged = true;
        hierarchy.nodes.clear();
    }

    Ok(hierarchy)
}

/// Computes a deterministic 64-bit FNV-1a fingerprint covering every node's `(index, visible_frame)`
/// so `WebDriver` invalidates `known_hash` short-circuits whenever container clipping changes even
/// when unclipped `frame` and `hittable` remain identical (without altering
/// `deja_flow::hash_hierarchy_nodes` Swift↔Rust parity).
pub(crate) fn clipped_geometry_hash(nodes: &[UiNode]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    #[inline]
    fn mix(mut hash: u64, bytes: &[u8]) -> u64 {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    #[inline]
    fn norm_bits(v: f64) -> [u8; 8] {
        let canonical = if !v.is_finite() || v == 0.0 { 0.0 } else { v };
        canonical.to_bits().to_le_bytes()
    }

    let mut hash = FNV_OFFSET_BASIS;
    hash = mix(hash, &(nodes.len() as u64).to_le_bytes());
    for node in nodes {
        hash = mix(hash, &(node.index as u64).to_le_bytes());
        match node.visible_frame {
            None => hash = mix(hash, &[0]),
            Some(rect) => {
                hash = mix(hash, &[1]);
                hash = mix(hash, &norm_bits(rect.x));
                hash = mix(hash, &norm_bits(rect.y));
                hash = mix(hash, &norm_bits(rect.width));
                hash = mix(hash, &norm_bits(rect.height));
            }
        }
    }
    hash
}

const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Decodes a standard RFC 4648 base64 PNG string from CDP `Page.captureScreenshot` and verifies
/// the 8-byte PNG header signature without external base64 crates.
pub(crate) fn decode_base64_png(b64: &str) -> Result<Vec<u8>> {
    let bytes = decode_base64(b64).map_err(WebDriverError::InvalidScreenshot)?;
    if bytes.len() < PNG_MAGIC.len() || &bytes[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(WebDriverError::InvalidScreenshot(
            "decoded screenshot bytes do not start with a valid PNG signature".to_string(),
        ));
    }
    Ok(bytes)
}

/// Pure-Rust RFC 4648 standard base64 decoder (ignores ASCII whitespace, strictly validates padding
/// and alphabet).
pub(crate) fn decode_base64(input: &str) -> std::result::Result<Vec<u8>, String> {
    let mut clean: Vec<u8> = Vec::with_capacity(input.len());
    for b in input.bytes() {
        if !b.is_ascii_whitespace() {
            clean.push(b);
        }
    }
    if clean.is_empty() {
        return Err("empty base64 payload".to_string());
    }
    if !clean.len().is_multiple_of(4) {
        return Err(format!(
            "invalid base64 length {} (must be a multiple of 4)",
            clean.len()
        ));
    }

    let mut out = Vec::with_capacity((clean.len() / 4) * 3);
    let total_chunks = clean.len() / 4;

    for (chunk_idx, chunk) in clean.as_chunks::<4>().0.iter().enumerate() {
        let is_last = chunk_idx + 1 == total_chunks;
        let a = decode_b64_digit(chunk[0])
            .ok_or_else(|| format!("invalid base64 byte 0x{:02x}", chunk[0]))?;
        let b = decode_b64_digit(chunk[1])
            .ok_or_else(|| format!("invalid base64 byte 0x{:02x}", chunk[1]))?;

        match (chunk[2], chunk[3]) {
            (b'=', b'=') => {
                if !is_last {
                    return Err("unexpected base64 padding before final quantum".to_string());
                }
                if (b & 0x0f) != 0 {
                    return Err("non-zero canonical trailing bits before `==` padding".to_string());
                }
                out.push((a << 2) | (b >> 4));
            }
            (c_byte, b'=') => {
                if !is_last {
                    return Err("unexpected base64 padding before final quantum".to_string());
                }
                let c = decode_b64_digit(c_byte)
                    .ok_or_else(|| format!("invalid base64 byte 0x{c_byte:02x}"))?;
                if (c & 0x03) != 0 {
                    return Err("non-zero canonical trailing bits before `=` padding".to_string());
                }
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
            }
            (b'=', _) => {
                return Err("malformed base64 padding `=` in third position".to_string());
            }
            (c_byte, d_byte) => {
                let c = decode_b64_digit(c_byte)
                    .ok_or_else(|| format!("invalid base64 byte 0x{c_byte:02x}"))?;
                let d = decode_b64_digit(d_byte)
                    .ok_or_else(|| format!("invalid base64 byte 0x{d_byte:02x}"))?;
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
                out.push((c << 6) | d);
            }
        }
    }

    Ok(out)
}

#[inline]
const fn decode_b64_digit(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
