(documentId, nodeId, direction) => {
  if (String(performance.timeOrigin) !== documentId) return {ok:false,reason:'document_changed'};
  const cache = window.__manuvra, region = cache?.nodes?.get(nodeId);
  if (!cache?.scrollConnected(region)) return {ok:false,reason:'target_missing'};
  const movable = e => cache.scrollEligible(e) && (direction < 0 ? e.scrollTop > 1 : e.scrollTop + e.clientHeight < e.scrollHeight - 1);
  if (!movable(region)) return {ok:false,reason:'scroll_region_at_end'};
  const r = cache.scrollVisibleRect(region);
  if (!r || r.width <= 0 || r.height <= 0) return {ok:false,reason:'covered'};
  for (const fy of [0.5,0.1,0.9,0.25,0.75]) for (const fx of [0.5,0.1,0.9]) {
    const x = r.x + r.width*fx, y = r.y + r.height*fy;
    let hit = cache.scrollHit(region,x,y);
    while (hit && !movable(hit)) hit = hit.parentElement || hit.getRootNode()?.host;
    if (hit === region) {
      const positions=[];let current=region;
      while (current) {
        if (cache.scrollEligible(current)) positions.push({node:current,identity:{name:cache.scrollName(current),overlay:cache.scrollOverlay(current),document:false},before:current.scrollTop});
        current=current.parentElement || current.getRootNode()?.host;
      }
      positions.push({node:null,identity:{overlay:null,document:true},before:scrollY});
      cache.scrollReadback={documentId,positions};
      return {ok:true,x,y,delta:direction*Math.max(Math.min(40,r.wheel_height/2),r.wheel_height-8)*r.wheel_scale};
    }
  }
  return {ok:false,reason:'covered'};
}
