const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs'), vm = require('node:vm');
const ts = require('../app/node_modules/typescript');
const moduleUnderTest = {exports:{}};
const styles = new Map();
const root = {classList:{add(){},remove(){}}};
vm.runInNewContext(ts.transpileModule(fs.readFileSync('app/src/theme/themeEngine.ts','utf8'),{
 compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}
}).outputText,{module:moduleUnderTest,exports:moduleUnderTest.exports,document:{
 documentElement:root,getElementById:id=>styles.get(id),
 createElement:()=>({remove(){styles.delete(this.id);}}),
 head:{appendChild:el=>styles.set(el.id,el)}
}});
test('primary foreground stays legible for dark and bright custom accents',()=>{
 const {primaryForeground}=moduleUnderTest.exports;
 for(const primary of ['#2244aa','#e5e5e5','#ffe99d','#292929','#fff','#111']){
  const color=primaryForeground(primary);
  const normalized=primary.length===4?'#'+[...primary.slice(1)].map(c=>c+c).join(''):primary;
  const luminance=hex=>{
   const rgb=[1,3,5].map(i=>parseInt(hex.slice(i,i+2),16)/255).map(c=>c<=.04045?c/12.92:((c+.055)/1.055)**2.4);
   return rgb[0]*.2126+rgb[1]*.7152+rgb[2]*.0722;
  };
  const a=luminance(normalized),b=luminance(color);
  assert.ok((Math.max(a,b)+.05)/(Math.min(a,b)+.05)>=4.5,primary+' '+color);
 }
});
test('custom theme emits its foreground token and removal restores base palette',()=>{
 const {applyTheme,removeCustomTheme}=moduleUnderTest.exports;
 applyTheme({id:'custom',name:'Dark blue',isDark:true,palette:{
 bg:'#141414',surface:'#1c1c1c',primary:'#2244aa',secondary:'#aaa',
 text:'#eee',subtext:'#aaa',border:'#333',hover:'#222'
 }});
 const css=styles.get('dynamic-theme').textContent;
 assert.match(css,/--tr-on-primary:\s*#ffffff/);
 removeCustomTheme();
 assert.equal(styles.size,0);
});
