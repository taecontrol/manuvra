(values) => {
  window.__manuvraRemoveMasks?.();
  // A channel can join retained nodes across roots; raw ranges cannot prove that assembly.
  const masks = [], matched = new Set(); let unverifiable = values.some(value => value.includes('\n'));
  const painted = (element) => element.checkVisibility({checkVisibilityCSS: true});
  const hasArea = (rect) => rect.width > 0 && rect.height > 0;
  const shown = (element) => painted(element) && hasArea(element.getBoundingClientRect());
  const up = node => node.assignedSlot || node.parentElement || node.getRootNode?.()?.host || null;
  const running = animation => animation.playState === 'running' || animation.pending;
  const independentAnimation = (animation, lineage) => {
    const target = animation.effect?.target;
    if (!target?.ownerDocument || lineage.has(target) || !animation.effect.getKeyframes) return false;
    const visual = ['offset','computedOffset','easing','composite','opacity','color','backgroundColor'];
    // Fixed visual effects cannot move surrounding flow. Unknown effects stay unverifiable.
    if (target.ownerDocument.defaultView.getComputedStyle(target).position === 'fixed') visual.push('transform','rotate','scale','translate');
    return animation.effect.getKeyframes().every(frame => Object.keys(frame).every(key => visual.includes(key)));
  };
  const stableTextPaint = (element) => {
    const lineage = new Set(), documents = new Set();
    for (let owner = element; owner; owner = up(owner) || owner.ownerDocument.defaultView.frameElement) {
      lineage.add(owner); documents.add(owner.ownerDocument);
      const css = owner.ownerDocument.defaultView.getComputedStyle(owner);
      // The root's filter also composites the masks; a lower filter can paint outside them.
      if (css.textShadow !== 'none' || css.webkitBoxReflect !== 'none' || css.filter !== 'none' && owner !== document.documentElement) return false;
    }
    // Documents omit shadow-tree effects; visited roots also reveal sibling layout motion.
    for (const {root} of contexts) {
      if (!documents.has(root.ownerDocument || root)) continue;
      if (root.getAnimations().some(animation => running(animation) && !independentAnimation(animation, lineage))) return false;
    }
    return true;
  };
  // Frame ranges use child CSS pixels. Offset-only masks require an unscaled parent space.
  const translatedSpace = (root) => {
    for (let element = root; element; element = up(element)) {
      const css = element.ownerDocument.defaultView.getComputedStyle(element);
      const matrix = new DOMMatrix(css.transform === 'none' ? undefined : css.transform);
      if (!matrix.is2D || matrix.a !== 1 || matrix.b !== 0 || matrix.c !== 0 || matrix.d !== 1 ||
          css.rotate !== 'none' || css.scale !== 'none' || parseFloat(css.zoom) !== 1 || css.perspective !== 'none') return false;
    }
    return true;
  };
  // A frame's document starts at its content box, inside the border and padding.
  const frameOrigin = (frame) => {
    const rect = frame.getBoundingClientRect(), style = frame.ownerDocument.defaultView.getComputedStyle(frame);
    return {x: rect.x + frame.clientLeft + parseFloat(style.paddingLeft), y: rect.y + frame.clientTop + parseFloat(style.paddingTop)};
  };

  const contexts = [], seen = new Set();
  const visit = (root, x, y) => {
    if (!root || seen.has(root)) return;
    seen.add(root); contexts.push({root, x, y});
    if (root.defaultView?.__manuvraClosedShadowRoots > 0) unverifiable = true;
    for (const element of root.querySelectorAll('*')) {
      // Document-level masks cannot cover a native top layer, regardless of their z-index.
      if (element.matches(':modal,:popover-open') || element === element.ownerDocument.fullscreenElement) unverifiable = true;
      if (element.shadowRoot) visit(element.shadowRoot, x, y);
      if (element.tagName === 'CANVAS' && shown(element)) unverifiable = true;
      if (element.tagName !== 'IFRAME' && element.tagName !== 'FRAME') continue;
      if (!shown(element)) continue;
      if (!translatedSpace(element)) { unverifiable = true; continue; }
      let child;
      try { child = element.contentDocument; } catch (_) { child = null; }
      if (!child?.body) { unverifiable = true; continue; }
      const origin = frameOrigin(element);
      visit(child, x + origin.x, y + origin.y);
    }
  };
  visit(document, 0, 0);
  // Native bounding rectangles prove coverage only when the mask itself stays axis-aligned.
  if (!translatedSpace(document.documentElement)) unverifiable = true;

  const cover = (rect, x, y) => {
    if (!rect || !Number.isFinite(rect.x) || !hasArea(rect)) { unverifiable = true; return; }
    const left = Math.floor(rect.x + x), top = Math.floor(rect.y + y);
    const right = Math.ceil(rect.x + x + rect.width), bottom = Math.ceil(rect.y + y + rect.height);
    const mask = document.createElement('div');
    mask.setAttribute('data-manuvra-mask', '');
    // Own the whole paint shape: author rules must not clip, round, blend or fade masks.
    for (const [name, value] of Object.entries({all:'initial',display:'block',position:'fixed',left:`${left}px`,top:`${top}px`,width:`${right-left}px`,height:`${bottom-top}px`,background:'#000','z-index':'2147483647','pointer-events':'none'})) mask.style.setProperty(name,value,'important');
    document.documentElement.appendChild(mask); masks.push(mask);
    // The root may establish another fixed-position containing block. Trust native coverage.
    const actual = mask.getBoundingClientRect(), css = window.getComputedStyle(mask);
    const generated = ['::before','::after'].some(pseudo => !['none','normal'].includes(window.getComputedStyle(mask,pseudo).content));
    if (generated || !mask.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}) || css.opacity !== '1' || css.backgroundColor !== 'rgb(0, 0, 0)' || !stableTextPaint(mask) ||
        actual.left > rect.x + x || actual.top > rect.y + y || actual.right < rect.x + x + rect.width || actual.bottom < rect.y + y + rect.height) unverifiable = true;
  };
  const coverRange = (range, context) => {
    const rects = [...range.getClientRects()].filter(hasArea);
    if (!rects.length) unverifiable = true;
    else rects.forEach(rect => cover(rect, context.x, context.y));
  };
  const matches = (text, coverMatch) => {
    values.forEach((value, index) => {
      if (!value) return;
      let start = 0, found;
      while ((found = String(text).indexOf(value, start)) !== -1) {
        matched.add(index); coverMatch(found, found + value.length);
        start = found + Math.max(1, value.length);
      }
    });
  };
  // A control paints its own value; a closed select paints its selected option's label.
  const controlText = (element) => element.tagName === 'SELECT'
    ? [...element.selectedOptions].map(option => option.label || option.textContent).join(' ')
    : String(element.value ?? element.innerText);
  // Text without layout (inside display:none, a hidden input, or head) is never painted;
  // a control's text is matched through the control itself.
  const laidOut = (node) => { const range = node.ownerDocument.createRange(); range.selectNodeContents(node); return range.getClientRects().length > 0; };
  // A laid-out range also paints when its parent is boxless or the shadow root itself.
  const unpaintedText = (node) => node.parentElement?.closest('script,style,noscript,template,textarea,select') || !laidOut(node);
  const boxless = (element) => !painted(element) && element.ownerDocument.defaultView.getComputedStyle(element).display !== 'contents';
  // Keep UTF-16 offsets into the raw node while applying the snapshot's whitespace rules.
  const normalizedText = (source) => {
    let text = ''; const starts = [], ends = [];
    for (const chunk of source.matchAll(/\s+|\S/g)) {
      const whitespace = /^\s/.test(chunk[0]), end = chunk.index + chunk[0].length;
      if (whitespace && (!text || end === source.length)) continue;
      text += whitespace ? ' ' : chunk[0]; starts.push(chunk.index); ends.push(end);
    }
    return {text, starts, ends};
  };

  for (const context of contexts) {
    for (const element of context.root.querySelectorAll('*')) {
      if (boxless(element)) continue;
      if (element.matches('input,textarea,select,[contenteditable="true"]') && hasArea(element.getBoundingClientRect())) {
        matches(controlText(element), () => { if (!stableTextPaint(element)) unverifiable = true; cover(element.getBoundingClientRect(), context.x, context.y); });
      }
      const view = element.ownerDocument?.defaultView || window;
      for (const pseudo of ['::before', '::after']) {
        matches(view.getComputedStyle(element, pseudo).content || '', () => cover(element.getBoundingClientRect(), context.x, context.y));
      }
    }
    const owner = context.root.ownerDocument || context.root;
    const walker = owner.createTreeWalker(context.root, NodeFilter.SHOW_TEXT), nodes = [];
    let text = '', node;
    while ((node = walker.nextNode())) {
      if (unpaintedText(node)) continue;
      const start = text.length; text += node.textContent || '';
      nodes.push({node, start, end: text.length});
      const normalized = normalizedText(node.textContent || '');
      matches(normalized.text, (start, end) => {
        if (!stableTextPaint(up(node))) unverifiable = true;
        const range = owner.createRange();
        range.setStart(node, normalized.starts[start]); range.setEnd(node, normalized.ends[end - 1]);
        coverRange(range, context);
      });
    }
    matches(text, (start, end) => {
      const first = nodes.find(item => item.start <= start && start < item.end), last = nodes.find(item => item.start < end && end <= item.end);
      if (!first || !last) { unverifiable = true; return; }
      if (nodes.some(item => item.end > start && item.start < end && !stableTextPaint(up(item.node)))) unverifiable = true;
      const range = owner.createRange();
      range.setStart(first.node, start - first.start); range.setEnd(last.node, end - last.start);
      coverRange(range, context);
    });
  }

  window.__manuvraRemoveMasks = () => { masks.forEach(mask => mask.remove()); delete window.__manuvraRemoveMasks; };
  return {verified: !unverifiable, sensitive_values_checked: values.length, matched_values: matched.size, mask_count: masks.length};
}
