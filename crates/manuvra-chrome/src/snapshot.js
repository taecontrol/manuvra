(() => {
  if (!document.body) return null;
  const cache = window.__manuvra ||= { ids: new WeakMap(), nodes: new Map(), next: 1 };
  const nodeId = (element) => {
    if (!cache.ids.has(element)) cache.ids.set(element, cache.next++);
    const id = cache.ids.get(element); cache.nodes.set(id, element); return id;
  };
  for (const [id, element] of cache.nodes) if (!element.isConnected) cache.nodes.delete(id);

  const gaps = [], contexts = [], seenRoots = new Set();
  // A frame's document starts at its content box, inside the border and padding.
  const frameOrigin = (frame) => {
    const rect = frame.getBoundingClientRect(), style = frame.ownerDocument.defaultView.getComputedStyle(frame);
    return {x: rect.x + frame.clientLeft + parseFloat(style.paddingLeft), y: rect.y + frame.clientTop + parseFloat(style.paddingTop)};
  };
  const visit = (root, context, offsetX, offsetY) => {
    if (!root || seenRoots.has(root)) return;
    seenRoots.add(root); contexts.push({root, context, offsetX, offsetY});
    if (root.defaultView?.__manuvraClosedShadowRoots > 0) gaps.push('closed_shadow_root');
    for (const element of root.querySelectorAll('*')) {
      if (element.shadowRoot) visit(element.shadowRoot, `${context}/shadow:${nodeId(element)}`, offsetX, offsetY);
      if (element.tagName !== 'IFRAME' && element.tagName !== 'FRAME') continue;
      let child;
      try { child = element.contentDocument; } catch (_) { child = null; }
      if (!child?.body) { gaps.push('cross_origin_frame'); continue; }
      const origin = frameOrigin(element);
      visit(child, `${context}/frame:${nodeId(element)}`, offsetX + origin.x, offsetY + origin.y);
    }
  };
  visit(document, 'main', 0, 0);

  const ancestors = (element) => {
    const values = []; let current = element;
    while (current) {
      values.push(current);
      const root = current.getRootNode?.();
      current = current.parentElement || root?.host || null;
    }
    return values;
  };
  const nearestDialog = (element) => ancestors(element).find(node => node.matches?.('dialog,[role="dialog"],[role="alertdialog"]')) || null;
  const viewOfElement = element => element.ownerDocument.defaultView;
  // Only ancestors below the containing block can be escaped by positioned descendants.
  const clippingRect = (element, context, viewport = true) => {
    const bounds = viewport ? {left:-context.offsetX, top:-context.offsetY, right:innerWidth-context.offsetX, bottom:innerHeight-context.offsetY} : {left:-Infinity,top:-Infinity,right:Infinity,bottom:Infinity};
    let current = element;
    while (current) {
      const style = viewOfElement(current).getComputedStyle(current);
      const positioned = style.position === 'absolute' || style.position === 'fixed';
      const parent = current.parentElement || current.getRootNode()?.host;
      let next = parent;
      if (positioned) next = current.offsetParent;
      if (!next || next.ownerDocument !== element.ownerDocument) break;
      const css = viewOfElement(next).getComputedStyle(next), r = next.getBoundingClientRect();
      // The body's overflow propagates to the viewport when the root leaves it visible.
      const rootStyle = viewOfElement(next).getComputedStyle(next.ownerDocument.documentElement);
      const viewportOverflow = next === next.ownerDocument.documentElement || (next === next.ownerDocument.body && rootStyle.overflowX === 'visible' && rootStyle.overflowY === 'visible');
      if (viewportOverflow) {current = next; continue;}
      if (css.overflowX !== 'visible') { bounds.left = Math.max(bounds.left,r.left+next.clientLeft); bounds.right = Math.min(bounds.right,r.left+next.clientLeft+next.clientWidth); }
      if (css.overflowY !== 'visible') { bounds.top = Math.max(bounds.top,r.top+next.clientTop); bounds.bottom = Math.min(bounds.bottom,r.top+next.clientTop+next.clientHeight); }
      current = next;
    }
    return bounds;
  };
  const inViewport = (element, context) => {
    const r = element.getBoundingClientRect(), b = clippingRect(element, context);
    return r.width > 0 && r.height > 0 && r.bottom > b.top && r.right > b.left && r.top < b.bottom && r.left < b.right;
  };
  const centerUnclipped = (element, context) => {
    const r = element.getBoundingClientRect(), b = clippingRect(element, context, false), x = r.x+r.width/2, y = r.y+r.height/2;
    // Keep the existing viewport-intersection rule; apply centers only to ancestor clipping.
    return x >= b.left && x < b.right && y >= b.top && y < b.bottom;
  };
  const rendered = (element, context) => Boolean(element) && element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}) && inViewport(element, context) && centerUnclipped(element, context);
  const visible = (element, context) => rendered(element, context) && !ancestors(element).some(node => node.matches?.('[aria-hidden="true"],[inert]'));
  const referencedById = (element, id) => element.getRootNode().getElementById?.(id);
  const name = (element, seen = new Set()) => {
    if (!element || seen.has(element)) return ''; seen.add(element);
    const labelled = (element.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean).map(id => name(referencedById(element, id), seen)).filter(Boolean).join(' ');
    return labelled || element.getAttribute('aria-label') || [...(element.labels || [])].map(label => name(label, seen)).filter(Boolean).join(' ') ||
      (['button','submit','reset'].includes(element.type) ? element.value : '') || element.getAttribute('alt') ||
      (element.tagName === 'INPUT' ? '' : [...element.childNodes].map(child => child.nodeType === 3 ? child.textContent : child.nodeType === 1 && child.getAttribute('aria-hidden') !== 'true' ? name(child, seen) : '').join(' ').replace(/\s+/g,' ').trim()) ||
      element.getAttribute('title') || element.getAttribute('placeholder') || '';
  };
  const role = (element) => {
    const explicit = element.getAttribute('role'); if (explicit) return explicit;
    if (element.tagName === 'IFRAME' || element.tagName === 'FRAME') return 'iframe';
    if (element.tagName === 'CANVAS') return 'canvas';
    if (element.tagName === 'DIALOG') return 'dialog';
    if (element.tagName === 'BUTTON' || element.tagName === 'SUMMARY') return 'button';
    if (element.tagName === 'A') return 'link'; if (element.tagName === 'SELECT') return 'combobox';
    if (element.tagName === 'TEXTAREA' || element.isContentEditable) return 'textbox';
    if (element.tagName === 'INPUT') {
      if (['checkbox','radio'].includes(element.type)) return element.type;
      if (['button','submit','reset','image'].includes(element.type)) return 'button';
      if (element.type === 'search') return 'searchbox'; if (element.type === 'number') return 'spinbutton'; return 'textbox';
    }
    return 'interactive';
  };
  const dialogTitle = (dialog) => {
    const labelled = (dialog.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean).map(id => referencedById(dialog, id)?.innerText?.replace(/\s+/g,' ').trim()).filter(Boolean).join(' ');
    return labelled || dialog.getAttribute('aria-label') || dialog.querySelector('h1,h2,h3,[role="heading"]')?.innerText?.replace(/\s+/g,' ').trim() || 'Untitled dialog';
  };

  const dialogRecords = [];
  for (const context of contexts) for (const dialog of context.root.querySelectorAll('dialog[open],[role="dialog"],[role="alertdialog"]')) {
    if (visible(dialog, context)) dialogRecords.push({dialog, context, title: dialogTitle(dialog)});
  }
  const dialogs = dialogRecords.map(record => record.title);
  const selector = 'a[href],button,input:not([type="hidden"]),textarea,select,summary,[contenteditable="true"],[role="button"],[role="link"],[role="checkbox"],[role="radio"],[role="switch"],[role="tab"],[role="menuitem"],[role="menuitemradio"],[role="option"],[role="combobox"],[role="textbox"],[role="searchbox"],[role="spinbutton"]';
  const CONTROL = selector;
  const up = (n) => n.parentElement || n.getRootNode?.()?.host || null;
  const isDialog = (n) => n.matches?.('dialog,[role="dialog"],[role="alertdialog"]');
  const nonLinkControl = (n) => n.matches?.(CONTROL) && !n.matches('a[href],[role="link"]');
  // Painted text segments of `root`, excluding text inside non-link controls and editable fields.
  const segments = (root, keepControls = false) => { const out = []; const w = (root.ownerDocument || document).createTreeWalker(root, NodeFilter.SHOW_TEXT); let t;
    while ((t = w.nextNode())) { const v = t.textContent.replace(/\s+/g, ' ').trim(), p = t.parentElement; if (!v || !p || p.closest('script,style,template')) continue;
      let q = p, skip = false; while (!keepControls && q && q !== root) { if (nonLinkControl(q) || q.matches?.('textarea,[contenteditable="true"]')) { skip = true; break; } q = up(q); }
      if (!keepControls && nonLinkControl(root)) skip = true;
      if (!skip && p.checkVisibility({checkVisibilityCSS:true})) out.push({node:t, text:v}); } return out; };
  const hasOwnText = (n) => segments(n).length > 0 || n.querySelector?.('h1,h2,h3,h4,h5,h6,[role="heading"]');
  const semantic = (n) => n.matches('li,tr,[role="row"],[role="listitem"],article,[role="article"],fieldset,form') ||
    ((n.hasAttribute('aria-label') || n.hasAttribute('aria-labelledby')) && !n.matches(CONTROL) && hasOwnText(n));
  const repeated = (n) => { if (n.matches('td,th,[role="cell"],[role="gridcell"],[role="columnheader"]')) return false; const parent = up(n); if (!parent || !parent.children) return false;
    const peers = [...parent.children].filter(s => s.tagName === n.tagName && s.querySelector(CONTROL));
    return peers.length >= 2 && peers.includes(n); };
  const containerNode = (control) => { let n = up(control);
    while (n && n.nodeType === 1 && n.tagName !== 'BODY' && n.tagName !== 'HTML') { if (isDialog(n)) return null;
      if (!n.matches('td,th,[role="cell"],[role="gridcell"],[role="columnheader"]') && (semantic(n) || repeated(n))) return n; n = up(n); } return null; };
  const containerLabel = (node) => { const byId = (node.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean).map(id => node.getRootNode().getElementById?.(id)?.textContent?.trim()).filter(Boolean).join(' ');
    const heading = node.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"]')?.textContent?.replace(/\s+/g,' ').trim();
    const first = segments(node)[0]?.text;
    let v = byId || node.getAttribute('aria-label') || heading || first;
    if (!v) { const all = segments(node.ownerDocument.body, true);
      const before = all.filter(x => !node.contains(x.node) && (node.compareDocumentPosition(x.node) & Node.DOCUMENT_POSITION_PRECEDING)).pop();
      const after = all.find(x => !node.contains(x.node) && (node.compareDocumentPosition(x.node) & Node.DOCUMENT_POSITION_FOLLOWING));
      v = `between “${(before?.text || 'start').slice(0,60)}” and “${(after?.text || 'end').slice(0,60)}”`; }
    return v.replace(/\s+/g, ' ').slice(0, 120); };
  const containerOf = (control) => { const node = containerNode(control); return node ? containerLabel(node) : null; };

  const elements = [], elementIndices = new Map(), seenElements = new Set(); let index = 1;
  for (const context of contexts) for (const element of context.root.querySelectorAll(selector)) {
    if (seenElements.has(element) || !visible(element, context)) continue; seenElements.add(element);
    const elementRole = role(element);
    const editable = !element.readOnly && element.getAttribute('aria-readonly') !== 'true' && (['textbox','searchbox','spinbutton'].includes(elementRole) || element.isContentEditable || (elementRole === 'combobox' && ['INPUT','TEXTAREA'].includes(element.tagName)));
    const operations = []; if (element.tagName === 'SELECT') operations.push('SELECT'); else if (editable) operations.push('TYPE_TEXT'); else operations.push('CLICK');
    const rect = element.getBoundingClientRect(), containingDialog = nearestDialog(element), current = index++;
    elementIndices.set(element, current); const boolAttr = key => element.hasAttribute(key) ? element.getAttribute(key) !== 'false' : null;
    const selectOptions = element.tagName === 'SELECT' ? [...element.options].map(option => ({node_id:nodeId(option),label:option.label||option.textContent.trim(),value:String(option.value),disabled:Boolean(option.disabled),selected:Boolean(option.selected)})) : [];
    elements.push({index:current,node_id:nodeId(element),context:context.context,role:elementRole,name:name(element)||elementRole,input_type:element.tagName==='INPUT'?element.type:null,value:'value' in element?String(element.value):(element.isContentEditable?element.innerText.trim():''),checked:'checked' in element?Boolean(element.checked):boolAttr('aria-checked'),selected:'selected' in element?Boolean(element.selected):boolAttr('aria-selected'),expanded:boolAttr('aria-expanded'),disabled:Boolean(element.disabled)||element.getAttribute('aria-disabled')==='true',in_dialog:containingDialog?dialogTitle(containingDialog):null,container:containerOf(element),operations,select_options:selectOptions,rect:{x:rect.x+context.offsetX,y:rect.y+context.offsetY,width:rect.width,height:rect.height}});
  }

  // Twins include opacity-hidden and offscreen controls, but never display:none controls.
  const twinCounts = new Map(), twinKey = (element) => JSON.stringify([role(element), (name(element) || role(element)).toLowerCase(), nearestDialog(element) ? dialogTitle(nearestDialog(element)) : null]);
  for (const context of contexts) for (const element of context.root.querySelectorAll(selector)) {
    if (!element.checkVisibility({checkVisibilityCSS:true})) continue;
    const key = twinKey(element); twinCounts.set(key, (twinCounts.get(key) || 0) + 1);
  }
  for (const [element, index] of elementIndices) elements[index - 1].shares_name = twinCounts.get(twinKey(element)) > 1;

  const REGION_LIMIT = 20, REGION_NAME_LIMIT = 120;
  const hiddenByOpacity = (element, context) => inViewport(element, context) && centerUnclipped(element, context) && element.checkVisibility({checkVisibilityCSS:true}) &&
    !element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}) && !ancestors(element).some(node => node.matches?.('[aria-hidden="true"],[inert]'));
  const revealingRegion = (element) => ancestors(element).slice(1).find(node => node.checkVisibility({checkOpacity:true}) &&
    (node.matches('li,tr,[role=row],[role=listitem]') || node.hasAttribute('aria-label')));
  // Opacity reveals inferred from CSSOM hover rules, importance, and layers.
  const hoverRules = [], baseRules = [], hideRules = []; let hoverRulesUnreadable = false, ruleOrder = 0;
  const viewOf = (node) => (node.ownerDocument || node).defaultView || window;
  for (const context of contexts) {
    const root = context.root, win = viewOf(root), layerOrder = new Map(), layerCounts = new Map(); let anonLayer = 0;
    // A child's position belongs to its parent; registering it never reorders root layers.
    const registerLayer = (name) => { let parent = '', path = [];
      for (const part of name.split('.')) { const qualified = parent ? parent + '.' + part : part;
        if (!layerOrder.has(qualified)) { const next = layerCounts.get(parent) || 0; layerCounts.set(parent, next + 1); layerOrder.set(qualified, [...path, next]); }
        path = layerOrder.get(qualified); parent = qualified; } };
    const sheets = [...(root.styleSheets || []), ...(root.adoptedStyleSheets || [])];
    const consider = (sel, style, layer) => { if (!style || style.opacity === '') return;
      const record = {s:sel, order:ruleOrder++, o:parseFloat(style.opacity), important:style.getPropertyPriority('opacity') === 'important', layer, layerOrder, root};
      if (/:hover(?![\w-])/.test(sel)) hoverRules.push(record); else if (Number.isFinite(record.o)) { baseRules.push(record); if (record.o === 0) hideRules.push(record); } };
    const walk = (rules, parent, layer) => { for (const r of rules) { try {
      if (win.CSSLayerStatementRule && r instanceof win.CSSLayerStatementRule) { for (const n of r.nameList) { const k = (layer ? layer + '.' : '') + n; registerLayer(k); } continue; }
      let childLayer = layer;
      if (win.CSSLayerBlockRule && r instanceof win.CSSLayerBlockRule) { childLayer = (layer ? layer + '.' : '') + (r.name || ('\0anon' + (anonLayer++))); registerLayer(childLayer); }
      if (win.CSSMediaRule && r instanceof win.CSSMediaRule && !win.matchMedia(r.media.mediaText).matches) continue;
      if (win.CSSSupportsRule && r instanceof win.CSSSupportsRule && !win.CSS.supports(r.conditionText)) continue;
      let own = null;
      if (win.CSSStyleRule && r instanceof win.CSSStyleRule) { own = parent ? (r.selectorText.includes('&') ? r.selectorText.replaceAll('&', `:is(${parent})`) : `:is(${parent}) ${r.selectorText}`) : r.selectorText; consider(own, r.style, layer); }
      if (win.CSSNestedDeclarations && r instanceof win.CSSNestedDeclarations && parent) consider(parent, r.style, layer);
      if (r.cssRules) walk(r.cssRules, own || parent, childLayer);
      } catch (_) { /* One unsupported rule must not discard the sheet. */ }
    } };
    for (const sheet of sheets) { try { if (!sheet.disabled && (!sheet.media?.mediaText || win.matchMedia(sheet.media.mediaText).matches)) walk(sheet.cssRules, null, null); } catch (_) { hoverRulesUnreadable = true; } }
  }
  const rank = (r) => { const path = r.layer == null ? [] : r.layerOrder.get(r.layer);
    if (r.inline) return r.important ? [3] : [1];
    // The implicit unlayered child comes last normally and first when importance reverses layers.
    return r.important ? [2, ...path.map(n => -n), -1e9] : [0, ...path, 1e9]; };
  const beats = (a, b) => { const x = rank(a), y = rank(b);
    for (let i = 0; i < Math.max(x.length, y.length); i++) { if (x[i] !== y[i]) return (x[i] ?? 0) > (y[i] ?? 0); }
    // Identical selectors have equal specificity, so their later declaration wins.
    // Comparing different selectors still keeps the bounded V3 specificity approximation.
    return a.s && a.s === b.s ? a.order >= b.order : true; };
  const safeMatches = (node, sel) => { try { return node.matches(sel); } catch (_) { return false; } };
  const siblingHover = (sel) => /:hover(?![\w-])[\s\S]*[~+]/.test(sel.replace(/\[[^\]]*\]|"[^"\\]*(?:\\.[^"\\]*)*"|'[^'\\]*(?:\\.[^'\\]*)*'/g, ''));
  const hoveredMatch = (node, rule) => rule.root === node.getRootNode() && !/:not\(\s*:hover\s*\)/.test(rule.s) && !siblingHover(rule.s) && safeMatches(node, rule.s.replace(/:hover(?![\w-])/g, ':is(*)'));
  const remainingOpacity = (node) => { const rules = [...baseRules.filter(r => r.root === node.getRootNode() && safeMatches(node, r.s)), ...hoverRules.filter(r => hoveredMatch(node, r))];
    const inline = parseFloat(node.style?.opacity);
    if (Number.isFinite(inline)) rules.push({o:inline, inline:true, important:node.style.getPropertyPriority('opacity') === 'important'});
    return rules.reduce((winner, rule) => !winner || beats(rule, winner) ? rule : winner, null)?.o ?? 1; };
  const revealRuleFor = (node) => hoverRules.find(rule => {
    if (rule.root !== node.getRootNode()) return false;
    if (/:not\(\s*:hover\s*\)/.test(rule.s)) return false;
    if (!(rule.o > 0) || siblingHover(rule.s) || !safeMatches(node, rule.s.replace(/:hover(?![\w-])/g, ':is(*)'))) return false;
    const hides = [...hideRules.filter(h => h.root === node.getRootNode() && safeMatches(node, h.s)), ...hoverRules.filter(h => h.o === 0 && hoveredMatch(node, h))];
    if (node.style?.opacity === '0') hides.push({inline:true, important:node.style.getPropertyPriority('opacity') === 'important'});
    return hides.every(h => beats(rule, h)); }) || hoverRules.find(rule => rule.root === node.getRootNode() && rule.o === 0 && /:not\(\s*:hover\s*\)/.test(rule.s) && !siblingHover(rule.s) && safeMatches(node, rule.s.replace(/:not\(\s*:hover\s*\)/g, ':is(*)')) && remainingOpacity(node) > 0);
  const opacityCarriers = (element) => ancestors(element).filter(node => node.nodeType === 1 && parseFloat(viewOf(node).getComputedStyle(node).opacity) === 0);
  const hoverRevealed = (element) => opacityCarriers(element).every(node => revealRuleFor(node));
  const unlabeledRegion = (element) => { const carriers = opacityCarriers(element), top = carriers[carriers.length - 1] || element;
    return ancestors(top).slice(1).find(node => node.nodeType === 1 && node.checkVisibility({checkOpacity:true}) && node.getBoundingClientRect().height > 0); };
  const paintedTexts = (root) => { const walker = (root.ownerDocument || document).createTreeWalker(root, NodeFilter.SHOW_TEXT), out = []; let t;
    while ((t = walker.nextNode())) { const v = t.textContent.replace(/\s+/g,' ').trim(), p = t.parentElement; if (v && p && !p.closest('script,style') && p.checkVisibility({checkOpacity:true,checkVisibilityCSS:true})) out.push({node:t, text:v}); } return out; };
  const regionName = (region) => { const own = paintedTexts(region).map(x => x.text).join(' ');
    if (region.getAttribute('aria-label')) return region.getAttribute('aria-label'); if (own) return own;
    const all = paintedTexts(region.ownerDocument.body);
    const before = all.filter(x => !region.contains(x.node) && (region.compareDocumentPosition(x.node) & Node.DOCUMENT_POSITION_PRECEDING)).pop();
    const after = all.find(x => !region.contains(x.node) && (region.compareDocumentPosition(x.node) & Node.DOCUMENT_POSITION_FOLLOWING));
    return `between “${(before?.text || 'start').slice(0,60)}” and “${(after?.text || 'end').slice(0,60)}”`; };

  const regionRecords = new Map(), seenHidden = new Set();
  for (const context of contexts) for (const element of context.root.querySelectorAll(selector)) {
    if (seenHidden.has(element) || !hiddenByOpacity(element, context)) continue; seenHidden.add(element);
    const labeled = revealingRegion(element);
    const revealRegion = labeled || (hoverRevealed(element) ? unlabeledRegion(element) : null); if (!revealRegion) continue;
    const container = containerNode(element), region = container || revealRegion;
    if (!regionRecords.has(region)) regionRecords.set(region, {name:(container ? containerLabel(container) : labeled ? (region.getAttribute('aria-label') || region.innerText.replace(/\s+/g,' ').trim()) : regionName(region)).slice(0,REGION_NAME_LIMIT),reveals_on_hover:[],reveal_roles:[],reveal_node_ids:[],node_id:nodeId(element)});
    const record = regionRecords.get(region);
    record.reveals_on_hover.push(name(element) || role(element));
    record.reveal_roles.push(role(element));
    record.reveal_node_ids.push(nodeId(element));
  }
  const hoverRegions = [...regionRecords.values()].slice(0, REGION_LIMIT).map((record, offset) => ({index:offset + 1, ...record}));

  const overlayCandidates = [];
  const containsAcrossRoots = (outer,inner) => ancestors(inner).includes(outer);
  const isolatedDialog = dialog => {
    const owner=dialog.ownerDocument;
    return [...owner.body.querySelectorAll('*')].filter(e=>!containsAcrossRoots(dialog,e) && !containsAcrossRoots(e,dialog) && !e.matches('script,style,template') && e.checkVisibility({checkOpacity:true,checkVisibilityCSS:true})).every(e=>ancestors(e).some(n=>n.matches?.('[aria-hidden="true"],[inert]')));
  };
  for (const context of contexts) {
    const controlled=new Set();
    for (const trigger of context.root.querySelectorAll('[aria-expanded="true"]')) {
      if (!visible(trigger,context) || !(trigger.getAttribute('aria-haspopup') && trigger.getAttribute('aria-haspopup') !== 'false' || trigger.getAttribute('role')==='combobox')) continue;
      for (const id of `${trigger.getAttribute('aria-controls') || ''} ${trigger.getAttribute('aria-owns') || ''}`.split(/\s+/).filter(Boolean)) {
        const element=referencedById(trigger,id);if(element) controlled.add(element);
      }
    }
    for (const element of context.root.querySelectorAll('*')) {
      if (!visible(element,context)) continue;
      const modal=element.matches('dialog:modal,[aria-modal="true"]:is(dialog,[role="dialog"],[role="alertdialog"])');
      const dialog=element.matches('dialog,[role="dialog"],[role="alertdialog"]');
      if (!modal && !controlled.has(element) && !element.matches(':popover-open') && !(dialog && isolatedDialog(element))) continue;
      const r=element.getBoundingClientRect(), x=r.x+r.width/2,y=r.y+r.height/2;
      const hit=context.root.elementFromPoint?.(x,y) || element.ownerDocument.elementFromPoint(x,y);
      if (hit && containsAcrossRoots(element,hit)) overlayCandidates.push({element,name:dialogTitle(element)});
    }
  }
  const topOverlay=overlayCandidates.at(-1) || null;
  const overlayOf=element=>overlayCandidates.filter(o=>containsAcrossRoots(o.element,element)).at(-1) || null;

  const scrollEligible = e => e?.nodeType === 1 && e !== e.ownerDocument.scrollingElement && !e.matches('body,html,input,textarea,select,[contenteditable="true"]') && !e.isContentEditable && ['auto','scroll'].includes(viewOf(e).getComputedStyle(e).overflowY) && e.scrollHeight > e.clientHeight;
  const scrollContexts = new Map();
  const scrollVisibleRect = element => {
    const context = scrollContexts.get(element.getRootNode()); if (!context) return null;
    const r = element.getBoundingClientRect(), b = clippingRect(element,context);
    const x = Math.max(r.x+element.clientLeft,b.left), y = Math.max(r.y+element.clientTop,b.top);
    const right = Math.min(r.x+element.clientLeft+element.clientWidth,b.right), bottom = Math.min(r.y+element.clientTop+element.clientHeight,b.bottom);
    return {x:x+context.offsetX,y:y+context.offsetY,width:Math.max(0,right-x),height:Math.max(0,bottom-y)};
  };
  for (const context of contexts) scrollContexts.set(context.root,context);
  cache.scrollEligible = scrollEligible; cache.scrollVisibleRect = scrollVisibleRect;
  cache.scrollHit = (region,x,y) => {
    const context = scrollContexts.get(region.getRootNode()), root = region.getRootNode();
    return root.elementFromPoint?.(x-context.offsetX,y-context.offsetY) || region.ownerDocument.elementFromPoint(x-context.offsetX,y-context.offsetY);
  };
  const scrollNodes = [];
  for (const context of contexts) for (const element of context.root.querySelectorAll('*')) {
    if (scrollEligible(element) && element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}) && inViewport(element,context) && !ancestors(element).some(n=>n.matches?.('[aria-hidden="true"],[inert]'))) scrollNodes.push(element);
  }
  const labelledName = element => {
    const ids = (element.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean);
    return ids.map(id=>name(referencedById(element,id))).filter(Boolean).join(' ') || element.getAttribute('aria-label') || element.getAttribute('title') || '';
  };
  const scrollName = element => labelledName(element) || [...element.querySelectorAll('[role="listbox"],[role="list"],table,[role="table"],[role="grid"],[role="menu"]')].map(labelledName).find(Boolean) || overlayOf(element)?.name || 'Scrollable area';
  const scrollRegions = scrollNodes.slice(0,REGION_LIMIT).map(element => ({
    node_id:nodeId(element),name:scrollName(element).slice(0,REGION_NAME_LIMIT),overlay:overlayOf(element)?.name || null,overlay_node_id:overlayOf(element)?nodeId(overlayOf(element).element):null,
    parent_node_id:(()=>{const p=ancestors(element).slice(1).find(n=>scrollNodes.includes(n));return p?nodeId(p):null})(),
    can_scroll_up:element.scrollTop>1,can_scroll_down:element.scrollTop+element.clientHeight<element.scrollHeight-1,
    scroll_top:element.scrollTop,scroll_height:element.scrollHeight,client_height:element.clientHeight,rect:scrollVisibleRect(element)
  }));

  const TEXT_LIMIT = 8000;
  const visibleText = [], coveredText = []; let visibleLength = 0, coveredLength = 0;
  const appendText = (parts, value, kind, length) => {
    const separator = parts.length ? 1 : 0, available = TEXT_LIMIT - length;
    if (available <= separator) { gaps.push(`${kind}_truncated`); return length; }
    const retained = value.slice(0, available - separator);
    if (retained) parts.push(retained);
    if (retained.length < value.length) gaps.push(`${kind}_truncated`);
    return length + separator + retained.length;
  };
  for (const context of contexts) {
    const owner = context.root.ownerDocument || context.root;
    const walker = owner.createTreeWalker(context.root, NodeFilter.SHOW_TEXT), range = owner.createRange(); let current;
    while ((current = walker.nextNode())) {
      const value = current.textContent.replace(/\s+/g,' ').trim(), parent = current.parentElement;
      if (!value || !parent || parent.closest('script,style,noscript,template')) continue;
      range.selectNodeContents(current); const bounds = clippingRect(parent, context);
      const intersects = [...range.getClientRects()].some(r => r.width > 0 && r.height > 0 && r.bottom > bounds.top && r.right > bounds.left && r.top < bounds.bottom && r.left < bounds.right);
      if (!intersects || !parent.checkVisibility({checkOpacity:true,checkVisibilityCSS:true})) continue;
      if (!ancestors(parent).some(node => node.matches?.('[aria-hidden="true"],[inert]'))) visibleLength = appendText(visibleText, value, 'visible_text', visibleLength);
      else coveredLength = appendText(coveredText, value, 'covered_text', coveredLength);
    }
  }
  let focused = null, focusAnchor = null, active = document.activeElement;
  for (const [element, elementIndex] of elementIndices) if (element.matches(':focus')) { focused = elementIndex; break; }
  while (active) {
    if (active.shadowRoot?.activeElement) { active = active.shadowRoot.activeElement; continue; }
    if (active.tagName === 'IFRAME' || active.tagName === 'FRAME') {
      let child;
      try { child = active.contentDocument; } catch (_) { child = null; }
      if (child?.activeElement) { active = child.activeElement; continue; }
    }
    break;
  }
  if (active && active !== active.ownerDocument.body && active !== active.ownerDocument.documentElement) {
    const root = active.getRootNode(), context = contexts.find(item => item.root === root);
    const indexed = elements.find(item => item.index === elementIndices.get(active));
    const containingDialog = nearestDialog(active);
    const dialog = containingDialog ? dialogTitle(containingDialog) : null;
    const owner = active.ownerDocument;
    const labelled = (active.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
      .map(id => referencedById(active, id)?.textContent?.replace(/\s+/g,' ').trim()).filter(Boolean).join(' ');
    const ariaName = labelled || active.getAttribute('aria-label') || active.getAttribute('title') || '';
    const view = owner.defaultView;
    const closed = Boolean(view?.__manuvraClosedShadowHosts?.has(active));
    const crossOrigin = (active.tagName === 'IFRAME' || active.tagName === 'FRAME') && !active.contentDocument;
    const boolAttr = key => active.hasAttribute(key) ? active.getAttribute(key) !== 'false' : null;
    const activeId = active.getAttribute('aria-activedescendant');
    const descendant = activeId && root.getElementById?.(activeId);
    const descendantBool = (element, key) => element?.hasAttribute(key) ? element.getAttribute(key) !== 'false' : null;
    const activeDescendant = activeId ? {id:activeId,role:descendant?role(descendant):'',name:descendant?name(descendant):'',selected:descendant && 'selected' in descendant ? Boolean(descendant.selected) : descendantBool(descendant,'aria-selected'),checked:descendant && 'checked' in descendant ? Boolean(descendant.checked) : descendantBool(descendant,'aria-checked')} : null;
    const anchorRole = indexed?.role || role(active);
    const posinset = Number.parseInt(active.getAttribute('aria-posinset'), 10);
    const container = active.parentElement?.closest('[role="tree"],[role="treegrid"],[role="grid"],[role="listbox"],[role="menu"],[role="menubar"],[role="tablist"],[role="radiogroup"],[role="toolbar"]') || active.parentElement;
    const siblingIndex = container ? [...container.querySelectorAll('*')].filter(element => role(element) === anchorRole).indexOf(active) + 1 : 0;
    const position = indexed ? null : posinset > 0 ? posinset : siblingIndex || null;
    focusAnchor = {active_descendant:activeDescendant,expanded:boolAttr('aria-expanded'),selected:'selected' in active ? Boolean(active.selected) : boolAttr('aria-selected'),checked:'checked' in active ? Boolean(active.checked) : boolAttr('aria-checked'),position,node_id:nodeId(active),context:context?.context || 'main',role:anchorRole,name:indexed?.name || (active === containingDialog ? dialog : ariaName),in_dialog:indexed?.in_dialog || dialog,container:containerOf(active),covered:!closed && !crossOrigin,surface:crossOrigin?'cross_origin_frame':closed?'closed_shadow_root':active.tagName==='CANVAS'?'canvas':null};
  }
  for (const context of contexts) for (const element of context.root.querySelectorAll('*')) {
    if (element.tagName === 'CANVAS') gaps.push('canvas');
    const view = element.ownerDocument?.defaultView || window;
    for (const pseudo of ['::before', '::after']) {
      const content = view.getComputedStyle(element, pseudo).content;
      if (content && !['none', 'normal', '""', "''"].includes(content)) gaps.push('generated_content');
    }
  }
  const dialogTexts = {};
  for (const record of dialogRecords) {
    const text = (record.dialog.innerText || '').replace(/\s+/g,' ').trim();
    if (text.length > TEXT_LIMIT) gaps.push('dialog_text_truncated');
    dialogTexts[record.title] = text.slice(0,TEXT_LIMIT);
  }
  const finalGaps = [...new Set(gaps)], truncated = finalGaps.some(gap => gap.endsWith('_truncated'));
  return {document_id:String(performance.timeOrigin),url:location.href,route:location.pathname+location.search,title:document.title,dialogs,focused,focus_anchor:focusAnchor,visible_text:visibleText.join('\n'),covered_text:coveredText.join('\n'),dialog_texts:dialogTexts,elements,viewport:{width:innerWidth,height:innerHeight,scroll_x:scrollX,scroll_y:scrollY,document_height:document.documentElement.scrollHeight},coverage:{viewport_complete:!truncated,open_shadow_roots:true,slots:true,same_origin_frames:!finalGaps.includes('cross_origin_frame'),gaps:finalGaps},overlay:topOverlay?{node_id:nodeId(topOverlay.element),name:topOverlay.name}:null,scroll_regions:scrollRegions,scroll_regions_truncated:scrollNodes.length>REGION_LIMIT,hover_rules_unreadable:hoverRulesUnreadable,hover_regions:hoverRegions,hover_regions_truncated:regionRecords.size > REGION_LIMIT};
})()
