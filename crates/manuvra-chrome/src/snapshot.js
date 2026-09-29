(() => {
  if (!document.body) return null;
  const cache = window.__manuvra ||= { ids: new WeakMap(), nodes: new Map(), next: 1 };
  const nodeId = (element) => {
    if (!cache.ids.has(element)) cache.ids.set(element, cache.next++);
    const id = cache.ids.get(element); cache.nodes.set(id, element); return id;
  };
  for (const [id, element] of cache.nodes) if (!element.isConnected) cache.nodes.delete(id);

  const gaps = [], contexts = [], seenRoots = new Set();
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
      const rect = element.getBoundingClientRect();
      visit(child, `${context}/frame:${nodeId(element)}`, offsetX + rect.x, offsetY + rect.y);
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
  const inViewport = (element, context) => {
    const r = element.getBoundingClientRect(), x = r.x + context.offsetX, y = r.y + context.offsetY;
    return r.width > 0 && r.height > 0 && y + r.height > 0 && x + r.width > 0 && y < innerHeight && x < innerWidth;
  };
  const rendered = (element, context) => Boolean(element) && element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}) && inViewport(element, context);
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
  const elements = [], elementIndices = new Map(), seenElements = new Set(); let index = 1;
  for (const context of contexts) for (const element of context.root.querySelectorAll(selector)) {
    if (seenElements.has(element) || !visible(element, context)) continue; seenElements.add(element);
    const elementRole = role(element);
    const editable = !element.readOnly && element.getAttribute('aria-readonly') !== 'true' && (['textbox','searchbox','spinbutton'].includes(elementRole) || element.isContentEditable || (elementRole === 'combobox' && ['INPUT','TEXTAREA'].includes(element.tagName)));
    const operations = []; if (element.tagName === 'SELECT') operations.push('SELECT'); else if (editable) operations.push('TYPE_TEXT'); else operations.push('CLICK');
    const rect = element.getBoundingClientRect(), containingDialog = nearestDialog(element), current = index++;
    elementIndices.set(element, current); const boolAttr = key => element.hasAttribute(key) ? element.getAttribute(key) !== 'false' : null;
    const selectOptions = element.tagName === 'SELECT' ? [...element.options].map(option => ({node_id:nodeId(option),label:option.label||option.textContent.trim(),value:String(option.value),disabled:Boolean(option.disabled),selected:Boolean(option.selected)})) : [];
    elements.push({index:current,node_id:nodeId(element),context:context.context,role:elementRole,name:name(element)||elementRole,input_type:element.tagName==='INPUT'?element.type:null,value:'value' in element?String(element.value):(element.isContentEditable?element.innerText.trim():''),checked:'checked' in element?Boolean(element.checked):boolAttr('aria-checked'),selected:'selected' in element?Boolean(element.selected):boolAttr('aria-selected'),expanded:boolAttr('aria-expanded'),disabled:Boolean(element.disabled)||element.getAttribute('aria-disabled')==='true',in_dialog:containingDialog?dialogTitle(containingDialog):null,operations,select_options:selectOptions,rect:{x:rect.x+context.offsetX,y:rect.y+context.offsetY,width:rect.width,height:rect.height}});
  }

  const REGION_LIMIT = 20, REGION_NAME_LIMIT = 120;
  const hiddenByOpacity = (element, context) => inViewport(element, context) && element.checkVisibility({checkVisibilityCSS:true}) &&
    !element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}) && !ancestors(element).some(node => node.matches?.('[aria-hidden="true"],[inert]'));
  const revealingRegion = (element) => ancestors(element).slice(1).find(node => node.checkVisibility({checkOpacity:true}) &&
    (node.matches('li,tr,[role=row],[role=listitem]') || node.hasAttribute('aria-label')));
  const regionRecords = new Map(), seenHidden = new Set();
  for (const context of contexts) for (const element of context.root.querySelectorAll(selector)) {
    if (seenHidden.has(element) || !hiddenByOpacity(element, context)) continue; seenHidden.add(element);
    const region = revealingRegion(element); if (!region) continue;
    if (!regionRecords.has(region)) regionRecords.set(region, {name:(region.getAttribute('aria-label') || region.innerText.replace(/\s+/g,' ').trim()).slice(0,REGION_NAME_LIMIT),reveals_on_hover:[],node_id:nodeId(element)});
    regionRecords.get(region).reveals_on_hover.push(name(element) || role(element));
  }
  const hoverRegions = [...regionRecords.values()].slice(0, REGION_LIMIT).map((record, offset) => ({index:offset + 1, ...record}));

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
      range.selectNodeContents(current); const rect = range.getBoundingClientRect(), x = rect.x + context.offsetX, y = rect.y + context.offsetY;
      if (!rect.width || !rect.height || y + rect.height <= 0 || x + rect.width <= 0 || y >= innerHeight || x >= innerWidth) continue;
      if (visible(parent, context)) visibleLength = appendText(visibleText, value, 'visible_text', visibleLength);
      else if (rendered(parent, context)) coveredLength = appendText(coveredText, value, 'covered_text', coveredLength);
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
    focusAnchor = {active_descendant:activeDescendant,expanded:boolAttr('aria-expanded'),selected:'selected' in active ? Boolean(active.selected) : boolAttr('aria-selected'),checked:'checked' in active ? Boolean(active.checked) : boolAttr('aria-checked'),position,node_id:nodeId(active),context:context?.context || 'main',role:anchorRole,name:indexed?.name || (active === containingDialog ? dialog : ariaName),in_dialog:indexed?.in_dialog || dialog,covered:!closed && !crossOrigin,surface:crossOrigin?'cross_origin_frame':closed?'closed_shadow_root':active.tagName==='CANVAS'?'canvas':null};
  }
  for (const context of contexts) for (const element of context.root.querySelectorAll('*')) {
    if (element.tagName === 'CANVAS') gaps.push('canvas');
    const view = element.ownerDocument?.defaultView || window;
    for (const pseudo of ['::before', '::after']) {
      const content = view.getComputedStyle(element, pseudo).content;
      if (content && !['none', 'normal', '""', "''"].includes(content)) gaps.push('generated_content');
    }
  }
  const uniqueGaps = [...new Set(gaps)];
  const dialogTexts = {};
  for (const record of dialogRecords) {
    const text = (record.dialog.innerText || '').replace(/\s+/g,' ').trim();
    if (text.length > TEXT_LIMIT) gaps.push('dialog_text_truncated');
    dialogTexts[record.title] = text.slice(0,TEXT_LIMIT);
  }
  const finalGaps = [...new Set(gaps)], truncated = finalGaps.some(gap => gap.endsWith('_truncated'));
  return {document_id:String(performance.timeOrigin),url:location.href,route:location.pathname+location.search,title:document.title,dialogs,focused,focus_anchor:focusAnchor,visible_text:visibleText.join('\n'),covered_text:coveredText.join('\n'),dialog_texts:dialogTexts,elements,viewport:{width:innerWidth,height:innerHeight,scroll_x:scrollX,scroll_y:scrollY,document_height:document.documentElement.scrollHeight},coverage:{viewport_complete:!truncated,open_shadow_roots:true,slots:true,same_origin_frames:!finalGaps.includes('cross_origin_frame'),gaps:finalGaps},hover_regions:hoverRegions,hover_regions_truncated:regionRecords.size > REGION_LIMIT};
})()
