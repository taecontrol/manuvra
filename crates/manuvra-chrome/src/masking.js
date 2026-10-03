(values) => {
  window.__manuvraRemoveMasks?.();
  const masks = [], matched = new Set(); let unverifiable = false;
  const painted = (element) => element.checkVisibility({checkVisibilityCSS: true});
  const hasArea = (rect) => rect.width > 0 && rect.height > 0;
  const shown = (element) => painted(element) && hasArea(element.getBoundingClientRect());
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
      if (element.shadowRoot) visit(element.shadowRoot, x, y);
      if (element.tagName === 'CANVAS' && shown(element)) unverifiable = true;
      if (element.tagName !== 'IFRAME' && element.tagName !== 'FRAME') continue;
      if (!shown(element)) continue;
      let child;
      try { child = element.contentDocument; } catch (_) { child = null; }
      if (!child?.body) { unverifiable = true; continue; }
      const origin = frameOrigin(element);
      visit(child, x + origin.x, y + origin.y);
    }
  };
  visit(document, 0, 0);

  const cover = (rect, x, y) => {
    if (!rect || !Number.isFinite(rect.x) || !hasArea(rect)) { unverifiable = true; return; }
    const mask = document.createElement('div');
    mask.setAttribute('data-manuvra-mask', '');
    Object.assign(mask.style, {position: 'fixed', left: `${rect.x + x}px`, top: `${rect.y + y}px`, width: `${rect.width}px`, height: `${rect.height}px`, background: '#000', zIndex: '2147483647', pointerEvents: 'none'});
    document.documentElement.appendChild(mask); masks.push(mask);
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

  for (const context of contexts) {
    for (const element of context.root.querySelectorAll('*')) {
      if (boxless(element)) continue;
      if (element.matches('input,textarea,select,[contenteditable="true"]') && hasArea(element.getBoundingClientRect())) {
        matches(controlText(element), () => cover(element.getBoundingClientRect(), context.x, context.y));
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
    }
    matches(text, (start, end) => {
      const first = nodes.find(item => item.start <= start && start < item.end), last = nodes.find(item => item.start < end && end <= item.end);
      if (!first || !last) { unverifiable = true; return; }
      const range = owner.createRange();
      range.setStart(first.node, start - first.start); range.setEnd(last.node, end - last.start);
      const rects = [...range.getClientRects()].filter(hasArea);
      if (!rects.length) unverifiable = true;
      else rects.forEach(rect => cover(rect, context.x, context.y));
    });
  }

  window.__manuvraRemoveMasks = () => { masks.forEach(mask => mask.remove()); delete window.__manuvraRemoveMasks; };
  return {verified: !unverifiable, sensitive_values_checked: values.length, matched_values: matched.size, mask_count: masks.length};
}
