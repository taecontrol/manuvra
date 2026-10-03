(() => {
  if (!document.body) return null;
  const cache = window.__manuvra ||= { ids: new WeakMap(), nodes: new Map(), next: 1 };
  const nodeId = (element) => {
    if (!cache.ids.has(element)) cache.ids.set(element, cache.next++);
    const id = cache.ids.get(element); cache.nodes.set(id, element); return id;
  };
  for (const [id, element] of cache.nodes) if (!element.isConnected) cache.nodes.delete(id);

  // Assigned nodes are painted through their slot, not through their light-DOM parent.
  const up = n => n.assignedSlot || n.parentElement || n.getRootNode?.()?.host || null;
  const gaps = [], contexts = [], seenRoots = new Set();
  let unverifiableFrameGeometry = false;
  // A frame's document starts at its content box, inside the border and padding.
  const frameOrigin = (frame) => {
    const rect = frame.getBoundingClientRect(), style = frame.ownerDocument.defaultView.getComputedStyle(frame);
    return {x: rect.x + frame.clientLeft + parseFloat(style.paddingLeft), y: rect.y + frame.clientTop + parseFloat(style.paddingTop)};
  };
  const mappedRect = (rect,matrix) => {
    const points = [[rect.x,rect.y],[rect.x+rect.width,rect.y],[rect.x,rect.y+rect.height],[rect.x+rect.width,rect.y+rect.height]].map(([x,y])=>matrix.transformPoint({x,y}));
    const x=Math.min(...points.map(p=>p.x)), y=Math.min(...points.map(p=>p.y));
    return {x,y,width:Math.max(...points.map(p=>p.x))-x,height:Math.max(...points.map(p=>p.y))-y};
  };
  const rectPoints = r => [{x:r.x,y:r.y},{x:r.x+r.width,y:r.y},{x:r.x+r.width,y:r.y+r.height},{x:r.x,y:r.y+r.height}];
  const pointBounds = points => {
    if(!points.length)return {x:0,y:0,width:0,height:0};
    const x=Math.min(...points.map(p=>p.x)),y=Math.min(...points.map(p=>p.y));
    return {x,y,width:Math.max(...points.map(p=>p.x))-x,height:Math.max(...points.map(p=>p.y))-y};
  };
  // Clip a polygon against each affine client edge in its own coordinate space.
  const clipEdge = (points,inverse,axis,limit,greater) => {
    const out=[],distance=p=>(inverse.transformPoint(p)[axis]-limit)*(greater?1:-1);
    if(!points.length)return out;
    let previous=points.at(-1),before=distance(previous);
    for(const point of points) {
      const after=distance(point);
      if((before>=0)!==(after>=0)) {
        const fraction=before/(before-after);
        out.push({x:previous.x+(point.x-previous.x)*fraction,y:previous.y+(point.y-previous.y)*fraction});
      }
      if(after>=0)out.push(point);
      previous=point;before=after;
    }
    return out;
  };
  const clippedPolygon = (points,bounds) => {
    const identity=new DOMMatrix();
    for(const [axis,limit,greater] of [['x',bounds.left,true],['x',bounds.right,false],['y',bounds.top,true],['y',bounds.bottom,false]])
      points=clipEdge(points,identity,axis,limit,greater);
    for(const {box,clipX,clipY} of bounds.clips) {
      const inverse=box.matrix.inverse();
      if(clipX) {points=clipEdge(points,inverse,'x',0,true);points=clipEdge(points,inverse,'x',box.width,false);}
      if(clipY) {points=clipEdge(points,inverse,'y',0,true);points=clipEdge(points,inverse,'y',box.height,false);}
    }
    return points;
  };
  const polygonColumnHeight = (polygon,x) => {
    const ys=[];
    for(let j=0;j<polygon.length;j++) {
      const a=polygon[j],b=polygon[(j+1)%polygon.length];
      if(x<Math.min(a.x,b.x) || x>Math.max(a.x,b.x))continue;
      if(a.x===b.x)ys.push(a.y,b.y);
      else ys.push(a.y+(b.y-a.y)*(x-a.x)/(b.x-a.x));
    }
    return Math.max(...ys)-Math.min(...ys);
  };
  const polygonArea = points => Math.abs(points.reduce((sum,p,i)=>{const q=points[(i+1)%points.length];return sum+p.x*q.y-q.x*p.y;},0))/2;
  // Keep client dimensions and DOMRects in explicit coordinate spaces.
  const elementGeometry = element => {
    let matrix = new DOMMatrix(), zoom = 1, svgBasis = false;
    for (let node=element;node;node=up(node)) {
      const css=node.ownerDocument.defaultView.getComputedStyle(node);
      const transform=new DOMMatrix(css.transform==='none'?undefined:css.transform);
      // A matrix3d used for a flat scale animation is still an affine X/Y map.
      const planar=[transform.m13,transform.m14,transform.m23,transform.m24,transform.m31,transform.m32,transform.m34].every(value=>value===0) && transform.m44===1;
      if (!planar || css.perspective!=='none') return null;
      let rotation=new DOMMatrix(), scaling=new DOMMatrix();
      if (css.rotate!=='none') {
        const parts=css.rotate.split(/\s+/), angle=parts.at(-1);
        if (parts.length>2 || parts.length===2 && parts[0]!=='z') return null;
        rotation=rotation.rotate(parseFloat(angle)*(angle.endsWith('turn')?360:angle.endsWith('rad')?180/Math.PI:1));
      }
      if (css.scale!=='none') {
        const parts=css.scale.split(/\s+/).map(Number);
        scaling=scaling.scale(parts[0],parts[1]??parts[0]);
      }
      const nodeZoom=parseFloat(css.zoom)||1;zoom*=nodeZoom;
      // SVG's native basis includes viewBox and attribute transforms, which CSS
      // transform alone cannot describe. Descendant HTML still uses CSS units.
      const view=node.ownerDocument.defaultView;
      if(!svgBasis && node instanceof view.SVGGraphicsElement && !(node instanceof view.SVGSVGElement)) {
        const native=node.getScreenCTM();if(!native)return null;
        const values=[native.a,native.b,native.c,native.d,native.e,native.f];
        if(!values.every(Number.isFinite))return null;
        matrix=new DOMMatrix(values).multiply(matrix);svgBasis=true;
      } else if(!svgBasis) matrix=new DOMMatrix().scale(nodeZoom).multiply(rotation).multiply(scaling).multiply(transform).multiply(matrix);
    }
    const css=element.ownerDocument.defaultView.getComputedStyle(element), rect=element.getBoundingClientRect();
    const width=parseFloat(css.width)+(css.boxSizing==='border-box'?0:parseFloat(css.paddingLeft)+parseFloat(css.paddingRight)+parseFloat(css.borderLeftWidth)+parseFloat(css.borderRightWidth));
    const height=parseFloat(css.height)+(css.boxSizing==='border-box'?0:parseFloat(css.paddingTop)+parseFloat(css.paddingBottom)+parseFloat(css.borderTopWidth)+parseFloat(css.borderBottomWidth));
    const linear=new DOMMatrix([matrix.a,matrix.b,matrix.c,matrix.d,0,0]);
    const box=mappedRect({x:0,y:0,width,height},linear);
    linear.e=rect.x-box.x;linear.f=rect.y-box.y;
    return Number.isFinite(linear.inverse().a)?{matrix:linear,zoom,width,height}:null;
  };
  const clientBox = element => {
    const geometry=elementGeometry(element);
    if(!geometry)throw new Error('Unverifiable client geometry');
    return {...geometry,matrix:geometry.matrix.translate(element.clientLeft,element.clientTop),width:element.clientWidth,height:element.clientHeight};
  };
  const overflowBox = (element,css) => {
    const box=clientBox(element);
    if(css.overflowX!=='clip' && css.overflowY!=='clip')return box;
    const value=css.overflowClipMargin,margin=parseFloat(value.split(/\s+/).at(-1))||0;
    let left=-margin,top=-margin,width=box.width+2*margin,height=box.height+2*margin;
    if(value.startsWith('content-box')) {
      left+=parseFloat(css.paddingLeft);top+=parseFloat(css.paddingTop);
      width-=parseFloat(css.paddingLeft)+parseFloat(css.paddingRight);
      height-=parseFloat(css.paddingTop)+parseFloat(css.paddingBottom);
    } else if(value.startsWith('border-box')) {
      left-=element.clientLeft;top-=element.clientTop;
      width+=parseFloat(css.borderLeftWidth)+parseFloat(css.borderRightWidth);
      height+=parseFloat(css.borderTopWidth)+parseFloat(css.borderBottomWidth);
    }
    return {...box,matrix:box.matrix.translate(left,top),width,height};
  };
  // Rebuild frame geometry at dispatch; a snapshot cannot authorize a later wheel.
  const frameContext = root => {
    let owner = root.ownerDocument || root;
    const frames = [];
    while (owner !== document) {
      const frame = owner.defaultView?.frameElement;
      if (!frame?.isConnected || frame.contentDocument !== owner) return null;
      frames.unshift({frame,owner}); owner = frame.ownerDocument;
    }
    let matrix=new DOMMatrix(),zoom=1;
    const viewports=[];
    for (const entry of frames) {
      entry.parentMatrix=matrix;
      const parent=entry.frame.ownerDocument.defaultView;
      viewports.push({matrix,width:parent.innerWidth,height:parent.innerHeight});
      const geometry=elementGeometry(entry.frame);if(!geometry)return null;
      const css=entry.frame.ownerDocument.defaultView.getComputedStyle(entry.frame);
      const local=geometry.matrix.translate(entry.frame.clientLeft+parseFloat(css.paddingLeft),entry.frame.clientTop+parseFloat(css.paddingTop));
      matrix=matrix.multiply(local);zoom*=geometry.zoom;
    }
    const view=(root.ownerDocument || root).defaultView;
    const viewportBounds={left:0,top:0,right:view.innerWidth,bottom:view.innerHeight};
    const viewportClips=viewports.map(box=>({box:{...box,matrix:matrix.inverse().multiply(box.matrix)},clipX:true,clipY:true}));
    return {matrix,zoom,viewportBounds,viewportClips,frames};
  };
  const hitAt = (region,x,y) => {
    const context=frameContext(region.getRootNode());if(!context)return null;
    const deepHit=(owner,x,y)=> {
      let hit=owner.elementFromPoint(x,y),next;
      while(hit?.shadowRoot && (next=hit.shadowRoot.elementFromPoint(x,y)) && next!==hit) hit=next;
      return hit;
    };
    for(const {frame,parentMatrix} of context.frames) {
      const point=parentMatrix.inverse().transformPoint({x,y});
      if(deepHit(frame.ownerDocument,point.x,point.y)!==frame)return null;
    }
    const point=context.matrix.inverse().transformPoint({x,y});
    return deepHit(region.ownerDocument,point.x,point.y);
  };
  const visit = (root, context, offsetX, offsetY) => {
    if (!root || seenRoots.has(root)) return;
    const geometry=frameContext(root);
    if(!geometry) {unverifiableFrameGeometry=true;return;}
    seenRoots.add(root); contexts.push({root, context, offsetX, offsetY, viewportBounds:geometry.viewportBounds,viewportClips:geometry.viewportClips,frameGeometry:geometry});
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
  if(unverifiableFrameGeometry)return null;

  const ancestors = (element) => {
    const values = []; let current = element;
    while (current) {
      values.push(current);
      current = up(current);
    }
    return values;
  };
  const nearestDialog = (element) => ancestors(element).find(node => node.matches?.('dialog,[role="dialog"],[role="alertdialog"]')) || null;
  const viewOfElement = element => element.ownerDocument.defaultView;
  // Only ancestors below the containing block can be escaped by positioned descendants.
  const clippingRect = (element, context, viewport = true, clipSelf = false) => {
    const windowBounds = context.viewportBounds || {left:-context.offsetX,top:-context.offsetY,right:innerWidth-context.offsetX,bottom:innerHeight-context.offsetY};
    const bounds = {...(viewport ? windowBounds : {left:-Infinity,top:-Infinity,right:Infinity,bottom:Infinity}),clips:viewport?[...(context.viewportClips || [])]:[]};
    let current = element;
    while (current) {
      const style = viewOfElement(current).getComputedStyle(current);
      const positioned = style.position === 'absolute' || style.position === 'fixed';
      const parent = up(current);
      let next = parent;
      if (positioned) next = current.offsetParent;
      if (clipSelf) {next = current; clipSelf = false;}
      if (!next || next.ownerDocument !== element.ownerDocument) break;
      const css = viewOfElement(next).getComputedStyle(next);
      // The body's overflow propagates to the viewport when the root leaves it visible.
      const rootStyle = viewOfElement(next).getComputedStyle(next.ownerDocument.documentElement);
      const viewportOverflow = next === next.ownerDocument.documentElement || (next === next.ownerDocument.body && rootStyle.overflowX === 'visible' && rootStyle.overflowY === 'visible');
      if (viewportOverflow) {current = next; continue;}
      // Overflow does not create a clipping box on contents or non-replaced inline elements.
      const boxless = css.display === 'contents' || next instanceof viewOfElement(next).SVGElement && !next.matches('svg,foreignObject') || css.display === 'inline' && next instanceof viewOfElement(next).HTMLElement && !next.matches('img,iframe,frame,object,embed,video,audio,canvas,input,textarea,select,button') && next.clientWidth === 0 && next.clientHeight === 0;
      if(!boxless && (css.overflowX !== 'visible' || css.overflowY !== 'visible'))
        bounds.clips.push({box:overflowBox(next,css),clipX:css.overflowX!=='visible',clipY:css.overflowY!=='visible'});
      current = next;
    }
    return bounds;
  };
  const inViewport = (element, context) => {
    const r = element.getBoundingClientRect(), b = clippingRect(element, context);
    return r.width > 0 && r.height > 0 && polygonArea(clippedPolygon(rectPoints(r),b)) > 0;
  };
  const centerUnclipped = (element, context) => {
    const r = element.getBoundingClientRect(), b = clippingRect(element, context, false), x = r.x+r.width/2, y = r.y+r.height/2;
    // Keep the existing viewport-intersection rule; apply centers only to ancestor clipping.
    return b.clips.every(({box,clipX,clipY})=>{const point=box.matrix.inverse().transformPoint({x,y});return (!clipX || point.x>=0 && point.x<box.width) && (!clipY || point.y>=0 && point.y<box.height);});
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

  const COLOR_LIMIT = 512, colors = [], colorScopes = [], seenColorScopes = new Set(), normalizedColors = new Map();
  let colorsComplete = true, colorCanvas;
  // Convert an opaque native color, then quantize alpha separately. Drawing the original
  // translucent color loses straight RGB through the canvas's premultiplied storage.
  // Only computed serializations enter this boundary; unsupported syntax stays unresolved.
  const canonicalColor = raw => {
    if (normalizedColors.has(raw)) return normalizedColors.get(raw);
    let rgba = null;
    try {
      let opaque = raw, alpha = 1;
      const slash = raw.match(/\/\s*([+-]?(?:\d*\.)?\d+(?:e[+-]?\d+)?)\s*\)$/i);
      if (slash) { alpha = Number(slash[1]); opaque = raw.replace(/\/[^)]+\)$/, '/ 1)'); }
      else if (raw.startsWith('rgba(')) {
        const parts = raw.slice(5, -1).split(',');
        if (parts.length !== 4) throw new Error('unsupported computed rgba');
        alpha = Number(parts.pop()); opaque = 'rgb(' + parts.join(',') + ')';
      } else if (raw.includes('/')) throw new Error('unsupported computed alpha');
      if (!Number.isFinite(alpha)) throw new Error('unsupported computed alpha');
      colorCanvas ||= new OffscreenCanvas(1, 1).getContext('2d', {colorSpace:'srgb'});
      colorCanvas.fillStyle = '#010203'; colorCanvas.fillStyle = opaque; const first = colorCanvas.fillStyle;
      colorCanvas.fillStyle = '#040506'; colorCanvas.fillStyle = opaque;
      if (first !== colorCanvas.fillStyle) throw new Error('unsupported canvas color');
      colorCanvas.clearRect(0, 0, 1, 1); colorCanvas.fillRect(0, 0, 1, 1);
      const pixel = colorCanvas.getImageData(0, 0, 1, 1).data;
      if (pixel[3] !== 255) throw new Error('unsupported opaque conversion');
      rgba = [...pixel].slice(0, 3).concat(Math.round(Math.max(0, Math.min(1, alpha)) * 255));
    } catch (_) { /* A normalization failure must not become a guessed color. */ }
    normalizedColors.set(raw, rgba); return rgba;
  };
  // Color paint crosses embedding documents; existing accessible text/control rules stay separate.
  const colorAncestors = element => {
    const nodes=[];
    for(let node=element;node;node=up(node) || node.ownerDocument.defaultView.frameElement)nodes.push(node);
    return nodes;
  };
  const colorVisible = element => {
    const css=viewOfElement(element).getComputedStyle(element);
    if(css.visibility!=='visible')return false;
    if(css.display!=='contents')return element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true});
    const owner=ancestors(element).find(node=>viewOfElement(node).getComputedStyle(node).display!=='contents');
    return Boolean(owner?.checkVisibility({checkOpacity:true}));
  };
  const colorEligible = element => colorVisible(element) && !colorAncestors(element).some(node =>
    node.matches?.('[inert]') || ((node.tagName==='IFRAME' || node.tagName==='FRAME') && !colorVisible(node)));
  const finiteColorBox = box => Number.isFinite(box.width) && Number.isFinite(box.height) &&
    [box.matrix.a,box.matrix.b,box.matrix.c,box.matrix.d,box.matrix.e,box.matrix.f].every(Number.isFinite);
  const appendColorClips = (element, bounds, mapping) => {
    let complete=true;
    for(const node of ancestors(element)) {
      const css=viewOfElement(node).getComputedStyle(node);
      // A boxless element has no clip shape; its descendants still paint through their ranges.
      if(css.display==='contents')continue;
      if(css.clipPath==='none' && (css.clip==='auto' || !['absolute','fixed'].includes(css.position)))continue;
      const geometry=elementGeometry(node);
      if(!geometry || !finiteColorBox(geometry)){complete=false;continue;}
      const box = (left,top,right,bottom) => bounds.clips.push({box:{matrix:mapping.multiply(geometry.matrix).translate(left,top),width:Math.max(0,right-left),height:Math.max(0,bottom-top)},clipX:true,clipY:true});
      if(css.clip!=='auto' && ['absolute','fixed'].includes(css.position)) {
        const parts=css.clip.match(/^rect\(([^)]+)\)$/)?.[1].split(/[,\s]+/).filter(Boolean);
        if(parts?.length===4) {
          const edge=(value,full)=>value==='auto'?full:parseFloat(value);
          box(edge(parts[3],0),edge(parts[0],0),edge(parts[1],geometry.width),edge(parts[2],geometry.height));
        } else complete=false;
      }
      if(css.clipPath!=='none') {
        const inset=css.clipPath.match(/^inset\(([^)]+)\)$/)?.[1].trim().split(/\s+/);
        if(!inset || inset.length>4 || !inset.every(value=>/^[+-]?(?:\d*\.)?\d+(?:px|%)$/.test(value))) {complete=false;continue;}
        const expanded=[inset[0],inset[1]??inset[0],inset[2]??inset[0],inset[3]??inset[1]??inset[0]];
        const edge=(value,full)=>parseFloat(value)*(value.endsWith('%')?full/100:1);
        box(edge(expanded[3],geometry.width),edge(expanded[0],geometry.height),geometry.width-edge(expanded[1],geometry.width),geometry.height-edge(expanded[2],geometry.height));
      }
    }
    return complete;
  };
  const colorPaint = (element, context, rects) => {
    const bounds=clippingRect(element,context,true,true), geometry=context.frameGeometry;
    let complete=appendColorClips(element,bounds,new DOMMatrix());
    for(const entry of geometry.frames) {
      const mapping=geometry.matrix.inverse().multiply(entry.parentMatrix);
      const frameBounds=clippingRect(entry.frame,{},false,true);
      for(const clip of frameBounds.clips)bounds.clips.push({...clip,box:{...clip.box,matrix:mapping.multiply(clip.box.matrix)}});
      complete=appendColorClips(entry.frame,bounds,mapping) && complete;
    }
    bounds.clips=bounds.clips.filter(clip=>{
      if(finiteColorBox(clip.box))return true;
      complete=false;return false;
    });
    return {complete,intersects:rects.some(r=>r.width>0 && r.height>0 && polygonArea(clippedPolygon(rectPoints(r),bounds))>0)};
  };
  const addColorScope = (node, context, kind) => {
    if (!node || seenColorScopes.has(node)) return;
    seenColorScopes.add(node);
    if (colorScopes.length >= COLOR_LIMIT) { colorsComplete = false; return; }
    const dialog = kind === 'dialog' ? null : nearestDialog(node);
    colorScopes.push({node_id:nodeId(node),context:context.context,name:kind === 'dialog'?dialogTitle(node):containerLabel(node),kind,dialog_node_id:dialog?nodeId(dialog):null});
  };
  const addColor = (element, context, text, indexed, rects = [element.getBoundingClientRect()]) => {
    if (!colorEligible(element)) return;
    const paint=colorPaint(element,context,rects);
    if(!paint.intersects)return;
    if (colors.length >= COLOR_LIMIT || (text?.length || 0) > 8000) { colorsComplete = false; return; }
    const dialog = nearestDialog(element), container = text && !isDialog(element) && (semantic(element) || repeated(element)) ? element : containerNode(element);
    addColorScope(dialog, context, 'dialog'); addColorScope(container, context, 'container');
    const raw = viewOfElement(element).getComputedStyle(element).color;
    colors.push({node_id:nodeId(element),context:context.context,text,name:indexed?.name || null,role:indexed?.role || null,
      in_dialog:dialog?dialogTitle(dialog):null,container:container?containerLabel(container):null,
      dialog_node_id:dialog?nodeId(dialog):null,container_node_id:container?nodeId(container):null,
      channel:colorAncestors(element).some(node=>node.matches?.('[aria-hidden="true"]'))?'painted_aria_hidden':'accessible',paint_complete:paint.complete,
      color:{raw,rgba:canonicalColor(raw)}});
  };

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
    addColor(element, context, null, elements.at(-1));
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
      const frame=frameContext(element.getRootNode()), point=frame?.matrix.transformPoint({x,y});
      const hit=point && hitAt(element,point.x,point.y);
      if (hit && containsAcrossRoots(element,hit)) overlayCandidates.push({element,name:dialogTitle(element)});
    }
  }
  const topOverlay=overlayCandidates.at(-1) || null;
  const overlayOf=element=>overlayCandidates.filter(o=>containsAcrossRoots(o.element,element)).at(-1) || null;

  const scrollEligible = e => e?.nodeType === 1 && e !== e.ownerDocument.scrollingElement && !e.matches('body,html,input,textarea,select,[contenteditable="true"]') && !e.isContentEditable && ['auto','scroll'].includes(viewOf(e).getComputedStyle(e).overflowY) && e.scrollHeight > e.clientHeight;
  const scrollContexts = new Map();
  // A diagonal viewport cut has different vertical spans at different columns.
  // Overlap where a DOM-present control can be revealed, rather than just where
  // the polygon is tallest. Rectangular regions keep their full visible height.
  const controlColumnHeight = (region, polygon, inverse, height) => {
    const roots=[region.ownerDocument];
    for(let i=0;i<roots.length;i++) for(const node of roots[i].querySelectorAll('*')) {
      if(node.shadowRoot)roots.push(node.shadowRoot);
      const lineage=ancestors(node);
      if(!node.matches(CONTROL) || !lineage.includes(region) || !node.checkVisibility({checkVisibilityCSS:true}) || lineage.some(n=>n.matches?.('[aria-hidden="true"],[inert]')))continue;
      // A supported CSS hover reveal can make a below-fold control actionable.
      if(!node.checkVisibility({checkOpacity:true}) && !(hoverRevealed(node) && (revealingRegion(node) || unlabeledRegion(node))))continue;
      const rect=node.getBoundingClientRect(),x=inverse.transformPoint({x:rect.x+rect.width/2,y:rect.y+rect.height/2}).x;
      const span=polygonColumnHeight(polygon,x);
      if(span>0)height=Math.min(height,span);
    }
    return height;
  };
  const scrollVisibleRect = element => {
    if (!scrollContexts.has(element.getRootNode())) return null;
    const context = frameContext(element.getRootNode()); if (!context) return null;
    const box=clientBox(element),b=clippingRect(element,context);
    const points=clippedPolygon(rectPoints({x:0,y:0,width:box.width,height:box.height}).map(p=>box.matrix.transformPoint(p)),b);
    const inverse=box.matrix.inverse(),polygon=points.map(p=>inverse.transformPoint(p));
    const rect=pointBounds(points.map(p=>context.matrix.transformPoint(p))),local=pointBounds(polygon);
    const height=controlColumnHeight(element,polygon,inverse,local.height);
    // Transforms change geometry; CSS zoom also changes the units of a delivered wheel.
    return {...rect,wheel_height:height,wheel_scale:context.zoom*box.zoom};
  };
  for (const context of contexts) scrollContexts.set(context.root,context);
  cache.scrollConnected = element => Boolean(element?.isConnected && frameContext(element.getRootNode()));
  cache.scrollEligible = scrollEligible; cache.scrollVisibleRect = scrollVisibleRect;
  cache.scrollHit = hitAt; cache.scrollParent = up;
  const scrollNodes = [];
  for (const context of contexts) for (const element of context.root.querySelectorAll('*')) {
    if (scrollEligible(element) && scrollVisibleRect(element) && element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}) && inViewport(element,context) && !ancestors(element).some(n=>n.matches?.('[aria-hidden="true"],[inert]'))) scrollNodes.push(element);
  }
  // DOM discovery order differs from painted order for assigned slot content.
  const scrollSet=new Set(scrollNodes);
  const scrollDepth=new Map(scrollNodes.map(node=>[node,ancestors(node).filter(parent=>scrollSet.has(parent)).length]));
  scrollNodes.sort((a,b)=>scrollDepth.get(a)-scrollDepth.get(b));
  const labelledName = element => {
    const ids = (element.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean);
    return ids.map(id=>name(referencedById(element,id))).filter(Boolean).join(' ') || element.getAttribute('aria-label') || element.getAttribute('title') || '';
  };
  const scrollName = element => labelledName(element) || [...element.querySelectorAll('[role="listbox"],[role="list"],table,[role="table"],[role="grid"],[role="menu"]')].map(labelledName).find(Boolean) || overlayOf(element)?.name || 'Scrollable area';
  cache.scrollName = scrollName; cache.scrollOverlay = element => overlayOf(element)?.name || null;
  const scrollRegions = scrollNodes.slice(0,REGION_LIMIT).map(element => ({
    node_id:nodeId(element),name:scrollName(element).slice(0,REGION_NAME_LIMIT),overlay:overlayOf(element)?.name || null,overlay_node_id:overlayOf(element)?nodeId(overlayOf(element).element):null,
    parent_node_id:(()=>{const p=ancestors(element).slice(1).find(n=>scrollNodes.includes(n));return p?nodeId(p):null})(),
    can_scroll_up:element.scrollTop>1,can_scroll_down:element.scrollTop+element.clientHeight<element.scrollHeight-1,
    scroll_top:element.scrollTop,scroll_height:element.scrollHeight,client_height:element.clientHeight,rect:scrollVisibleRect(element)
  }));

  const TEXT_LIMIT = 8000;
  // Text inventories have their own budget; the capped color-owner list cannot prove absence.
  const textInventory = () => ({accessible:'',painted_aria_hidden:'',complete:true});
  const viewportText = textInventory(), dialogTextInventories = new Map(dialogRecords.map(record=>[record.dialog,textInventory()]));
  const appendPaintedText = (inventory, channel, value) => {
    const separator = inventory[channel] ? '\n' : '', available = TEXT_LIMIT - inventory[channel].length;
    const addition = separator + value;
    // A UTF-16 cut through a surrogate pair would make the whole CDP JSON unreadable.
    const retained = addition.slice(0, Math.max(0,available)).replace(/[\uD800-\uDBFF]$/, '');
    inventory[channel] += retained;
    if (addition.length > available) inventory.complete = false;
  };
  const addPaintedText = (element, context, value, rects) => {
    if (!colorEligible(element)) return;
    const lineage = colorAncestors(element);
    const inventories = [viewportText,...lineage.filter(node=>dialogTextInventories.has(node)).map(node=>dialogTextInventories.get(node))];
    const paint = colorPaint(element,context,rects);
    if (!paint.complete) { for (const inventory of inventories) inventory.complete = false; return; }
    if (!paint.intersects) return;
    const rgba = canonicalColor(viewOfElement(element).getComputedStyle(element).color);
    if (!rgba) { for (const inventory of inventories) inventory.complete = false; return; }
    if (rgba[3] === 0) return;
    const channel = lineage.some(node=>node.matches?.('[aria-hidden="true"]')) ? 'painted_aria_hidden' : 'accessible';
    for (const inventory of inventories) appendPaintedText(inventory,channel,value);
  };
  const seenColorTexts = new Set();
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
      const paintedParent = current.assignedSlot || parent || current.getRootNode()?.host;
      if (value && paintedParent && !paintedParent.closest('script,style,noscript,template')) {
        range.selectNodeContents(current);
        addPaintedText(paintedParent,context,value,[...range.getClientRects()]);
      }
      if (!value || !parent || parent.closest('script,style,noscript,template')) continue;
      range.selectNodeContents(current); const bounds = clippingRect(parent, context, true, true);
      const intersects = [...range.getClientRects()].some(r => r.width > 0 && r.height > 0 && polygonArea(clippedPolygon(rectPoints(r),bounds)) > 0);
      if (!seenColorTexts.has(current)) { seenColorTexts.add(current); addColor(parent, context, value, null, [...range.getClientRects()]); }
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
    const colorKind=isDialog(element)?'dialog':semantic(element) || repeated(element)?'container':null;
    if (colorKind && colorEligible(element) && colorPaint(element, context, [element.getBoundingClientRect()]).intersects)
      addColorScope(element, context, colorKind);
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
  const paintedText = {viewport:viewportText,dialogs:Object.fromEntries(dialogRecords.map(record=>[record.title,dialogTextInventories.get(record.dialog)]))};
  return {document_id:String(performance.timeOrigin),url:location.href,route:location.pathname+location.search,title:document.title,dialogs,focused,focus_anchor:focusAnchor,visible_text:visibleText.join('\n'),covered_text:coveredText.join('\n'),dialog_texts:dialogTexts,painted_text:paintedText,elements,colors,colors_complete:colorsComplete,color_scopes:colorScopes,viewport:{width:innerWidth,height:innerHeight,scroll_x:scrollX,scroll_y:scrollY,document_height:document.documentElement.scrollHeight},coverage:{viewport_complete:!truncated,open_shadow_roots:true,slots:true,same_origin_frames:!finalGaps.includes('cross_origin_frame'),gaps:finalGaps},overlay:topOverlay?{node_id:nodeId(topOverlay.element),name:topOverlay.name}:null,scroll_regions:scrollRegions,scroll_regions_truncated:scrollNodes.length>REGION_LIMIT,hover_rules_unreadable:hoverRulesUnreadable,hover_regions:hoverRegions,hover_regions_truncated:regionRecords.size > REGION_LIMIT};
})()
