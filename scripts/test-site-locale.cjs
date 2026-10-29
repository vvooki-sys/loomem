const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const path = require('node:path');
const code = fs.readFileSync(path.join(__dirname, '../docs/assets/locale.js'), 'utf8');
class Link {
 constructor(href, language = null) { this.href=href; this.language=language; this.handlers={}; }
 hasAttribute(n) { return n==='data-language' && this.language !== null; }
 getAttribute(n) { return n==='data-language' ? this.language : null; }
 addEventListener(n,f) { this.handlers[n]=f; }
 click(options={}) { const e={button:0,preventDefault(){this.defaultPrevented=true;}, ...options}; this.handlers.click(e); return e; }
}
function run({href='https://loomem.ai/',lang='en',saved=null,languages=['en'],noStorage=false,links=[]}={}) {
 let next=null, set=null, callback;
 let parsed=new URL(href);
 const location={href,pathname:parsed.pathname,origin:parsed.origin,replace(x){next={type:'replace',url:x}},assign(x){next={type:'assign',url:x}}};
 const document={documentElement:{lang},addEventListener(n,f){if(n==='DOMContentLoaded')callback=f;},querySelectorAll(sel){return sel==='a[href]'?links:links.filter(x=>x.hasAttribute('data-language'));}};
 const localStorage={getItem(){if(noStorage) throw Error('blocked');return saved;},setItem(k,v){if(noStorage) throw Error('blocked');set=[k,v]}};
 vm.runInNewContext(code,{URL,location,document,localStorage,navigator:{languages,language:languages[0]}});
 if(callback) callback();
 return {get next(){return next},get set(){return set}};
}
function test(name,f){try{f();console.log('PASS',name)}catch(e){console.log('FAIL',name,e.message);process.exitCode=1}}
test('browser locale chooses Polish and preserves query/hash',()=>{const r=run({href:'https://loomem.ai/guide/configuration.html?a=1#search',languages:['pl-PL','en']});assert.deepEqual(r.next,{type:'replace',url:'https://loomem.ai/pl/guide/configuration.html?a=1#search'})});
test('stored English beats Polish browser',()=>{const r=run({href:'https://loomem.ai/pl/guide/configuration.html?a=1#search',lang:'pl',saved:'en',languages:['pl']});assert.equal(r.next.url,'https://loomem.ai/guide/configuration.html?a=1#search')});
test('unknown locales fall back to English',()=>{const r=run({href:'https://loomem.ai/pl/compare.html',lang:'pl',saved:'fr',languages:['de-DE']});assert.equal(r.next.url,'https://loomem.ai/compare.html')});
test('no redirect loop after automatic locale choice',()=>{const r=run({href:'https://loomem.ai/pl/guide/configuration.html?a=1#search',lang:'pl',languages:['pl-PL','en']});assert.equal(r.next,null)});
test('explicit switch saves selection and preserves query/hash',()=>{const l=new Link('https://loomem.ai/pl/guide/quick-start.html','pl');const r=run({href:'https://loomem.ai/guide/quick-start.html?a=1#more',links:[l]});assert.equal(l.click().defaultPrevented,true);assert.deepEqual(r.set,['loomem.locale','pl']);assert.equal(r.next.url,'https://loomem.ai/pl/guide/quick-start.html?a=1#more')});
test('blocked storage uses query marker',()=>{const l=new Link('https://loomem.ai/guide/quick-start.html','en');const r=run({href:'https://loomem.ai/pl/guide/quick-start.html?a=1#more',lang:'pl',languages:['pl'],links:[l],noStorage:true});l.click();assert.equal(r.next.url,'https://loomem.ai/guide/quick-start.html?a=1&loomem_lang=en#more');const next=run({href:r.next.url,lang:'en',languages:['pl'],noStorage:true});assert.equal(next.next,null)});
test('marker propagates to local links and external links untouched',()=>{const a=new Link('https://loomem.ai/guide/installation.html?source=a#first-run'); const b=new Link('https://example.com/guide/installation.html');const r=run({href:'https://loomem.ai/guide/?loomem_lang=en',languages:['pl'],noStorage:true,links:[a,b]}); assert.equal(r.next,null); assert.equal(a.href,'https://loomem.ai/guide/installation.html?source=a&loomem_lang=en#first-run');assert.equal(b.href,'https://example.com/guide/installation.html')});
test('PL page rewrites English home and docs to Polish',()=>{const a=new Link('https://loomem.ai/'); const b=new Link('https://loomem.ai/guide/installation.html#first-run');const r=run({href:'https://loomem.ai/pl/guide/',lang:'pl',languages:['pl'],links:[a,b]});assert.equal(a.href,'https://loomem.ai/pl/');assert.equal(b.href,'https://loomem.ai/pl/guide/installation.html#first-run')});
test('invalid marker ignored; valid storage chosen',()=>{const r=run({href:'https://loomem.ai/?loomem_lang=fr',saved:'pl'});assert.equal(r.next.url,'https://loomem.ai/pl/?loomem_lang=fr')});
test('modified clicks keep query/hash and open the chosen language without changing the original preference',()=>{
 for (const options of [{ctrlKey:true},{metaKey:true},{button:1}]) {
  const link=new Link('https://loomem.ai/pl/guide/quick-start.html','pl');
  const source=run({href:'https://loomem.ai/guide/quick-start.html?from=bookmarks#more',saved:'en',links:[link]});
  assert.equal(link.href,'https://loomem.ai/pl/guide/quick-start.html?from=bookmarks&loomem_lang=pl#more');
  const event=link.click(options);
  assert.notEqual(event.defaultPrevented,true);
  assert.equal(source.set,null);
  assert.equal(source.next,null);
  const opened=run({href:link.href,lang:'pl',saved:'en',languages:['en']});
  assert.equal(opened.next,null);
 }
});
test('opening a language link without firing click works even when storage is blocked',()=>{
 const link=new Link('https://loomem.ai/guide/quick-start.html','en');
 run({href:'https://loomem.ai/pl/guide/quick-start.html?from=bookmarks#more',lang:'pl',languages:['pl'],noStorage:true,links:[link]});
 assert.equal(link.href,'https://loomem.ai/guide/quick-start.html?from=bookmarks&loomem_lang=en#more');
 const follow=new Link('https://loomem.ai/guide/installation.html');
 const opened=run({href:link.href,lang:'en',languages:['pl'],noStorage:true,links:[follow]});
 assert.equal(opened.next,null);
 assert.equal(follow.href,'https://loomem.ai/guide/installation.html?loomem_lang=en');
});
test('normal switch replaces a stale query marker and still stores the choice',()=>{
 const link=new Link('https://loomem.ai/guide/quick-start.html','en');
 const source=run({href:'https://loomem.ai/pl/guide/quick-start.html?loomem_lang=pl&from=bookmarks#more',lang:'pl',saved:'en',links:[link]});
 assert.equal(link.href,'https://loomem.ai/guide/quick-start.html?loomem_lang=en&from=bookmarks#more');
 link.click();
 assert.deepEqual(source.set,['loomem.locale','en']);
 assert.equal(source.next.url,'https://loomem.ai/guide/quick-start.html?from=bookmarks#more');
});
