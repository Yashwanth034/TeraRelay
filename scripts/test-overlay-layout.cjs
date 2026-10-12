const fs = require('node:fs');
const path = require('node:path');
const root = path.resolve(__dirname, '..');
const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
const dockClass = source.slice(source.indexOf('{(uploadQueue.length > 0')).match(/className="([^"]+)"/)[1];
const output = process.argv[2];
if (!output) throw new Error('Pass the HTML fixture output path');
const css = fs.readFileSync(path.join(root, 'app/src/App.css'), 'utf8');
const html = `<!doctype html><html><head><style>
html,body{margin:0;width:100%;height:100%}
.fixed{position:fixed}.relative{position:relative}
.inset-0{inset:0}.bottom-24{bottom:6rem}.right-5{right:1.25rem}
.h-screen{height:100vh}.w-full{width:100%}.flex{display:flex}
.flex-col{flex-direction:column}.flex-1{flex:1;min-width:0}
.overflow-hidden{overflow:hidden}.overflow-y-auto{overflow-y:auto}
[class~="w-[380px]"]{width:380px}
[class~="max-h-[calc(100vh-8rem)]"]{max-height:calc(100vh - 8rem)}
[class~="max-w-[calc(100vw-2.5rem)]"]{max-width:calc(100vw - 2.5rem)}
*{box-sizing:border-box}
${css}
</style></head><body>
<div class="terarelay-shell flex h-screen w-full overflow-hidden relative">
<aside class="tr-sidebar" style="width:240px">Sidebar</aside>
<main class="tr-main-panel flex-1">Channel</main>
<div id="dock" class="${dockClass}"><div class="tr-transfer-dock" style="height:300px">Uploads and downloads</div></div>
<div id="modal" class="tr-modal-backdrop fixed inset-0 flex" style="align-items:center;justify-content:center;z-index:200">
<div class="tr-modal" style="width:620px;max-width:calc(100vw - 32px);height:400px;max-height:calc(100vh - 32px)">Remote Import</div>
</div></div>
<script>
requestAnimationFrame(()=>requestAnimationFrame(()=>{
const rect=e=>{const r=e.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height,right:r.right,bottom:r.bottom}};
const dock=document.querySelector('#dock'), modal=document.querySelector('#modal'), panel=modal.firstElementChild;
const d=rect(dock),m=rect(modal),p=rect(panel);
const checks={
dockFixed:getComputedStyle(dock).position==='fixed',
modalFixed:getComputedStyle(modal).position==='fixed',
dockWithinViewport:d.x>=0&&d.y>=0&&d.right<=innerWidth+1&&d.bottom<=innerHeight+1,
modalCoversViewport:Math.abs(m.x)<1&&Math.abs(m.y)<1&&Math.abs(m.width-innerWidth)<1&&Math.abs(m.height-innerHeight)<1,
panelWithinViewport:p.x>=0&&p.y>=0&&p.right<=innerWidth+1&&p.bottom<=innerHeight+1,
shellContentUnchanged:document.querySelector('main').getBoundingClientRect().width>=innerWidth-280
};
window.__qaReport={pass:Object.values(checks).every(Boolean),checks,dock:d,modal:m,panel:p,viewport:[innerWidth,innerHeight]};
}));
</script></body></html>`;
fs.writeFileSync(output,html);
console.log('Overlay fixture generated',output);
