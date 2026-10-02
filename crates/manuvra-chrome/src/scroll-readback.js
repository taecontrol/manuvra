(async () => {
  const captured=window.__manuvra?.scrollReadback;
  if (!captured || String(performance.timeOrigin)!==captured.documentId) return null;
  const read=()=>captured.positions.map(p=>({...p.identity,before:p.before,after:p.node?p.node.scrollTop:scrollY}));
  let previous=null;
  for (let frame=0;frame<60;frame++) {
    await new Promise(resolve=>requestAnimationFrame(resolve));
    if (captured.positions.some(p=>p.node && !p.node.isConnected)) return null;
    const positions=read(), current=JSON.stringify(positions);
    if (current===previous) {delete window.__manuvra.scrollReadback;return positions;}
    previous=current;
  }
  delete window.__manuvra.scrollReadback;
  return null;
})()
