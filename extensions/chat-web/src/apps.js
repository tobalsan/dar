(function () {
  'use strict';
  if (window.DarMcpApps) return;
  const views = new Map();
  const autoOpen = new Set();
  const normalize = name => { for(const tool of ['artifact_publish','web_preview'])if([`dar.${tool}`,`dar_${tool}`].includes(name))return tool;const dot=typeof name==='string'?name.indexOf('.'):-1;return dot>=0 && name.slice(dot+1).includes('__')?name.slice(dot+1):name; };
  const isPreview = descriptor => descriptor.server==='dar' && descriptor.resourceUri==='ui://dar/web-preview';
  const withoutFragment = value => {const url=new URL(value);url.hash='';return url.href;};
  let settings;
  const request = async (url, options = {}) => {
    const response = await fetch(url, { ...options, signal: options.signal || AbortSignal.timeout(32000) });
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || 'App unavailable');
    return body;
  };
  const configuration = () => settings || (settings = request('/chat/apps/config'));
  const post = (view, message) => view.frame?.contentWindow?.postMessage(message, view.origin);
  const notify = (view, method, params) => { if (view.ready) post(view, { jsonrpc: '2.0', method, params }); };
  const context = view => ({ availableDisplayModes: ['inline', 'fullscreen'], displayMode: view.mode,
    containerDimensions: view.mode === 'fullscreen' ? {width:view.frame.clientWidth,height:view.frame.clientHeight} : {width:view.frame.clientWidth,maxHeight:800}, platform:'web' });
  const deliver = view => {
    if (!view.ready || !view.data || view.dataRecord!==view.record || view.delivered === view.record) return;
    if(view.inputCallId!==view.callId) notify(view, 'ui/notifications/tool-input', { arguments: view.data.args });
    view.inputCallId=view.callId;
    notify(view, 'ui/notifications/tool-result', view.data.result);
    view.delivered = view.record;
    if(autoOpen.delete(view.record) && innerWidth>760) mode(view,'fullscreen');
  };
  const audit = (view, method, params) => request(`/chat/${encodeURIComponent(view.slot)}/apps/${encodeURIComponent(view.record)}/rpc`, {
    method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify({jsonrpc:'2.0',id:1,method,params})
  });
  const ICONS={
    globe:'<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="4" ry="9"/><path d="M3 12h18"/></svg>',
    inline:'<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/></svg>',
    download:'<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3v12M7 10l5 5 5-5M5 21h14"/></svg>',
    browser:'<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 3h6v6M10 14L21 3M10 3H3v18h18v-7"/></svg>',
    file:'<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M14 2H5v20h14V7zM14 2v5h5M8 12h8M8 16h8"/></svg>',
    image:'<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="8" cy="8" r="2"/><path d="m3 18 6-6 4 4 4-6 4 5"/></svg>',
    code:'<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m8 6-6 6 6 6M16 6l6 6-6 6M14 3l-4 18"/></svg>',
    fullscreen:'<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7"/></svg>'};
  const artifactInfo = descriptor => {
    const mime=(descriptor.artifactMediaType || '').split(';')[0].trim().toLowerCase();
    const extension=(descriptor.artifactFilename || '').split('.').pop().toLowerCase();
    const types={'text/html':'HTML','text/markdown':'Markdown','application/pdf':'PDF','image/png':'PNG','image/jpeg':'JPEG','image/gif':'GIF','image/webp':'WebP','image/svg+xml':'SVG'};
    const extensions={html:'HTML',htm:'HTML',md:'Markdown',markdown:'Markdown',pdf:'PDF',png:'PNG',jpg:'JPEG',jpeg:'JPEG',gif:'GIF',webp:'WebP',svg:'SVG'};
    const text=mime.startsWith('text/') || ['application/json','application/javascript','application/xml','application/x-sh','application/toml','application/yaml','application/x-yaml'].includes(mime);
    const type=types[mime] || extensions[extension] || (text || /^(txt|log|rs|py|js|cjs|mjs|ts|tsx|jsx|json|yaml|yml|toml|css|sh|bash|zsh|sql|go|java|c|h|cpp|hpp|cs|rb|php|swift|xml|csv)$/.test(extension)?'Text':'File');
    const bytes=descriptor.artifactBytes;
    const size=Number.isFinite(bytes)?(bytes<1024?`${bytes} B`:bytes<1048576?`${Number((bytes/1024).toFixed(1))} KB`:`${Number((bytes/1048576).toFixed(1))} MB`):'';
    return {type,size,supported:mime?Boolean(types[mime] || text):type!=='File',icon:['PNG','JPEG','GIF','WebP','SVG'].includes(type)?'image':['HTML','Markdown','Text'].includes(type)?'code':'file'};
  };
  const heading = (view,descriptor) => {
    if(view.server!=='dar')return;
    if(isPreview(descriptor)) {
      const host=new URL(descriptor.previewUrl).hostname,title=descriptor.previewTitle || host;
      view.name.textContent=title;view.frame.title=title;view.previewUrl=descriptor.previewUrl;
      view.icon.innerHTML=ICONS.globe;view.details.textContent=host;view.open.hidden=false;
      if(view.card)view.card.replaceChildren(view.icon.cloneNode(true),document.createTextNode(`${title} Open in side panel`));
      return;
    }
    const title=descriptor.artifactTitle || descriptor.artifactFilename || 'Artifact';
    view.name.textContent=title;view.frame.title=title;
    const info=artifactInfo(descriptor);
    view.icon.innerHTML=ICONS[info.icon];view.details.textContent=[info.type,info.size].filter(Boolean).join(' · ');
    view.open.hidden=!info.supported;
    if(view.card) {view.card.replaceChildren(view.icon.cloneNode(true),document.createTextNode(`${title} Open in side panel`));}
    view.download.href=`/chat/${encodeURIComponent(view.slot)}/apps/${encodeURIComponent(descriptor.recordId)}/download`;
    view.download.title=`Download ${descriptor.artifactFilename || title}`;
  };
  const label = view => {
    const text=view.mode==='fullscreen'?'Close expanded view':'Expand view';
    view.button.innerHTML=ICONS[view.mode];
    view.button.title=text;
    view.button.setAttribute('aria-label',`${text}: ${view.server}`);
  };
  const layoutPanel = () => {
    const root=document.getElementById('chat-root');
    if (!root) return;
    const rect=root.getBoundingClientRect();
    root.style.setProperty('--app-top',`${rect.top}px`);
    root.style.setProperty('--app-height',`${rect.height}px`);
  };
  const attention = view => {
    view.section.classList.add('is-attention');view.frame.focus();
    setTimeout(()=>view.section.classList.remove('is-attention'),1200);
  };
  const mode = (view, wanted) => {
    if (!['inline','fullscreen'].includes(wanted) || !view.modes.includes(wanted)) return view.mode;
    if (wanted==='fullscreen') {
      for (const other of views.values()) if (other!==view && other.mode==='fullscreen') mode(other,'inline');
    }
    view.mode = wanted;
    view.section.classList.toggle('is-expanded', wanted==='fullscreen');
    if(view.server==='dar') {
      if(wanted==='fullscreen') {
        if(!view.card) {
          view.card=document.createElement('button');view.card.type='button';view.card.className='chat-app-card';
          view.card.append(view.icon.cloneNode(true),document.createTextNode(`${view.name.textContent} Open in side panel`));
          view.card.onclick=()=>attention(view);
        }
        view.section.before(view.card);
      } else {view.card?.remove();view.card=null}
    }
    const root=document.getElementById('chat-root');
    root?.classList.toggle('has-expanded-app',Array.from(views.values()).some(other=>other.mode==='fullscreen' && !other.destroying));
    if (wanted==='fullscreen') layoutPanel();
    label(view);
    notify(view,'ui/notifications/host-context-changed',context(view));
    return view.mode;
  };
  const fail = (view,error) => {
    clearTimeout(view.timer);
    view.status.textContent=`View unavailable: ${error.message || error}. Text result remains above.`;
    view.frame.hidden=true;
  };
  const teardown = async view => {
    if(view.destroying)return;
    view.destroying=true;
    clearTimeout(view.timer);
    view.observer?.disconnect();
    const anchor=view.section.parentElement;
    if(anchor?.dataset.appAnchor!==undefined)anchor.dataset.appRetiring='';
    if(view.ready) {
      post(view,{jsonrpc:'2.0',id:'dar-teardown',method:'ui/resource-teardown',params:{reason:'Conversation changed'}});
      await new Promise(resolve=>{view.teardownDone=resolve;setTimeout(resolve,250)});
    }
    mode(view,'inline');
    if(anchor?.dataset.appRetiring!==undefined)anchor.remove();else view.section.remove();
    views.delete(view.key);
    sync(view.app);
  };
  const valid = message => message && typeof message==='object' && !Array.isArray(message) && message.jsonrpc==='2.0'
    && (message.id===undefined || typeof message.id==='string' || Number.isSafeInteger(message.id))
    && (message.method===undefined || (typeof message.method==='string' && (message.params===undefined || (message.params && typeof message.params==='object' && !Array.isArray(message.params)))));
  window.addEventListener('message',async event=>{
    const view=Array.from(views.values()).find(view=>event.source===view.frame.contentWindow && event.origin===view.origin);
    if(!view || view.destroying && event.data?.id!=='dar-teardown')return;
    const message=event.data;
    if(!valid(message)) {
      audit(view,'invalid-message',{}).catch(()=>{});
      if(view.ready && message && (typeof message.id==='string' || Number.isSafeInteger(message.id))) post(view,{jsonrpc:'2.0',id:message.id,error:{code:-32600,message:'Malformed JSON-RPC message'}});
      return;
    }
    if(message.id==='dar-teardown' && !message.method){view.teardownDone?.();return}
    const params=message.params || {};
    const reply=result=>post(view,{jsonrpc:'2.0',id:message.id,result});
    const reject=error=>post(view,{jsonrpc:'2.0',id:message.id,error:{code:-32000,message:error.message || String(error)}});
    if(message.method==='ui/notifications/sandbox-proxy-ready') {
      if(view.bootstrapped)return;
      view.bootstrapped=true;
      post(view,{jsonrpc:'2.0',method:'ui/notifications/sandbox-resource-ready',params:{html:view.data.html,csp:view.data.meta?.ui?.csp,permissions:view.data.meta?.ui?.permissions}});
      return;
    }
    if(message.method==='ui/initialize' && message.id!==undefined && !view.initializing) {
      audit(view,'ui/initialize',params).catch(()=>{});
      if(typeof params.protocolVersion!=='string') {reject(new Error('Invalid MCP Apps protocol version'));return}
      const modes=params.appCapabilities?.availableDisplayModes;
      if(modes!==undefined && (!Array.isArray(modes) || !modes.every(mode=>typeof mode==='string'))) {reject(new Error('Invalid display modes'));return}
      view.modes=modes || ['inline','fullscreen'];
      view.initializing=true;
      reply({protocolVersion:'2026-01-26',hostInfo:{name:'Dar',version:'1'},hostCapabilities:{openLinks:{},serverTools:{},serverResources:{},logging:{},sandbox:{csp:view.data.meta?.ui?.csp || {},permissions:view.data.meta?.ui?.permissions || {}}},hostContext:context(view)});
      return;
    }
    if(message.method==='ui/notifications/initialized' && view.initializing && !view.ready) {
      audit(view,'ui/notifications/initialized',{}).catch(()=>{});
      view.ready=true;view.frame.hidden=false;clearTimeout(view.timer);view.status.textContent='';view.button.hidden=!view.modes.includes('fullscreen');deliver(view);return;
    }
    if(!view.ready)return;
    try {
      if(message.method==='ui/notifications/size-changed') {
        audit(view,'ui/notifications/size-changed',{}).catch(()=>{});
        if(Number.isFinite(params.height) && params.height>=0 && view.mode==='inline') view.frame.style.height=`${Math.max(40,Math.min(800,params.height))}px`;
      } else if(message.method==='ui/request-display-mode' && message.id!==undefined) {
        await audit(view,message.method,params);reply({mode:mode(view,params.mode)});
      } else if(message.method==='ui/open-link' && message.id!==undefined) {
        const response=await audit(view,message.method,params);
        if(response.error)throw new Error(response.error.message);
        const url=new URL(params.url);
        if(!['http:','https:'].includes(url.protocol))throw new Error('Only HTTP and HTTPS links are allowed');
        window.open(url.href,'_blank','noopener,noreferrer');reply({});
      } else if(['ping','tools/call','resources/read','ui/message','ui/update-model-context','notifications/message'].includes(message.method)) {
        const response=await audit(view,message.method,params);
        if(message.id!==undefined) {if(response.error)reject(new Error(response.error.message));else reply(response.result)}
      } else if(message.id!==undefined && message.method) {
        audit(view,message.method,params).catch(()=>{});
        reject(new Error('Unsupported app method'));
      }
    } catch(error) {if(message.id!==undefined)reject(error)}
  });
  const create = async (app,descriptor,anchor,key) => {
    const view={key,app,server:descriptor.server,slot:app.slot,record:descriptor.recordId,callId:descriptor.callId,piToolName:descriptor.piToolName,artifactFilename:descriptor.artifactFilename,mode:'inline',modes:['inline'],ready:false};
    views.set(key,view);
    const section=document.createElement('section');section.className='chat-app';section.setAttribute('aria-label',`MCP App: ${descriptor.server}`);
    const header=document.createElement('header');const name=document.createElement('strong');name.textContent=`${descriptor.server} · MCP App`;
    const button=document.createElement('button');button.type='button';button.className='chat-app-expand';button.hidden=true;
    const download=descriptor.server==='dar' && !isPreview(descriptor)?document.createElement('a'):null;
    const open=descriptor.server==='dar'?document.createElement('button'):null;
    let icon,details;
    const actions=document.createElement('div');actions.className='chat-app-actions';
    if(download){download.className='chat-app-download';download.download='';download.innerHTML=ICONS.download;download.title='Download';download.setAttribute('aria-label','Download');actions.append(download);}
    if(open){open.type='button';open.className='chat-app-open';open.innerHTML=ICONS.browser;open.title='Open in browser';open.setAttribute('aria-label','Open in browser');actions.append(open);}
    actions.append(button);
    if(descriptor.server==='dar') {
      icon=document.createElement('span');icon.className='chat-app-type-icon';icon.setAttribute('aria-hidden','true');
      details=document.createElement('small');
      const copy=document.createElement('div');copy.className='chat-app-file';copy.append(name,details);header.append(icon,copy,actions);
    } else header.append(name,actions);
    const status=document.createElement('p');status.setAttribute('role','status');status.textContent='Loading view…';
    const frame=document.createElement('iframe');frame.title=`${descriptor.server} app`;frame.setAttribute('sandbox',`allow-scripts allow-same-origin${isPreview(descriptor)?' allow-forms':''}`);frame.style.height='300px';
    section.append(header,status,frame);anchor.append(section);Object.assign(view,{section,frame,button,status,name,download,open,icon,details,tool:descriptor.tool});heading(view,descriptor);label(view);
    // Send fresh dimensions whenever the expanded frame changes size (layout settle, window resize, divider drag).
    let last='';
    view.observer=new ResizeObserver(()=>{
      if(view.mode!=='fullscreen')return;
      const size=`${view.frame.clientWidth}x${view.frame.clientHeight}`;
      if(size===last)return;
      last=size;notify(view,'ui/notifications/host-context-changed',context(view));
    });
    view.observer.observe(frame);
    button.onclick=()=>mode(view,view.mode==='inline'?'fullscreen':'inline');
    if(open)open.onclick=async()=>{
      if(view.previewUrl){window.open(view.previewUrl,'_blank','noopener,noreferrer');return}
      const tab=window.open('about:blank');
      if(!tab){view.status.textContent='Allow pop-ups to open the file';return}
      tab.opener=null;view.status.textContent='';
      try {
        const data=await request(`/chat/${encodeURIComponent(view.slot)}/apps/${encodeURIComponent(view.record)}/open`,{signal:AbortSignal.timeout(10000)});
        const url=new URL(data.url);
        if(url.origin!==view.origin)throw new Error('Invalid sandbox assignment');
        tab.location.href=url.href;
      } catch(error) {
        tab.close();view.status.textContent=error.name==='TimeoutError'?'Opening the file timed out':`Unable to open file: ${error.message || error}`;
      }
    };
    const divider=document.createElement('div');divider.className='chat-app-resizer';divider.setAttribute('role','separator');divider.setAttribute('aria-label','Resize app panel');divider.setAttribute('aria-orientation','vertical');section.append(divider);
    divider.onpointerdown=event=>{
      divider.setPointerCapture(event.pointerId);
      divider.onpointermove=event=>{const root=document.getElementById('chat-root');root?.style.setProperty('--app-panel-width',`${Math.max(320,Math.min(innerWidth-400,innerWidth-event.clientX))}px`)};
      divider.onpointerup=()=>{divider.onpointermove=null};
    };
    try {
      const [config,data]=await Promise.all([configuration(),request(`/chat/${encodeURIComponent(app.slot)}/apps/${encodeURIComponent(descriptor.recordId)}`)]);
      if(view.destroying || isPreview(descriptor) && view.record!==descriptor.recordId)return view;
      if(config.sandboxPort==null)throw new Error('Sandbox listener unavailable');
      const origin=new URL(data.sandboxOrigin);
      if(!['http:','https:'].includes(origin.protocol) || origin.hostname===location.hostname)throw new Error('App and chat must use separate hostnames. Open local chat at 127.0.0.1.');
      view.origin=origin.origin;view.noReuse=config.noReuse.includes(descriptor.server);
      if(view.record===descriptor.recordId || !view.data) {view.data=data;view.dataRecord=descriptor.recordId;}
      const url=new URL(data.sandboxUrl);
      if(url.origin!==origin.origin)throw new Error('Invalid sandbox assignment');
      frame.src=url.href;
      view.timer=setTimeout(()=>fail(view,new Error('App initialization timed out')),15000);
    }catch(error){if(!isPreview(descriptor) || view.record===descriptor.recordId)fail(view,error)}
    return view;
  };
  const sync = async app => {
    const config=await configuration().catch(()=>null);if(!config)return;
    const groups=new Map();
    for(const block of app.blocks) if(block.kind==='tool' && block.app) {
      const d=block.app,key=d.server==='dar'?JSON.stringify([app.slot,d.server,d.resourceUri,isPreview(d)?withoutFragment(d.previewUrl):d.artifactFilename,...(config.noReuse.includes(d.server)?[d.recordId]:[])]):`${app.slot}:${d.server}:${d.resourceUri}${config.noReuse.includes(d.server)?':'+d.recordId:''}`;
      if(!groups.has(key))groups.set(key,[]);groups.get(key).push(d);
    }
    for(const view of views.values())if(view.slot!==app.slot || !groups.has(view.key))teardown(view);
    const anchorOf=d=>document.querySelector(`[data-app-anchor="${CSS.escape(d.recordId)}"]`);
    for(const [key,descriptors] of groups) {
      const latest=descriptors.at(-1),follows=latest.server==='dar',anchor=anchorOf(follows?latest:descriptors[0]);
      if(!anchor)continue;
      let view=views.get(key);
      if(!view) {create(app,latest,anchor,key);view=views.get(key)}
      if(follows && view.section.parentElement!==anchor) {
        view.card?.remove();
        anchor.querySelector('[data-app-marker]')?.remove();
        anchor.append(view.section);
        if(view.mode==='fullscreen')mode(view,'fullscreen');
        // Moving an iframe reloads it, so run the app handshake again.
        Object.assign(view,{bootstrapped:false,initializing:false,ready:false,delivered:null,inputCallId:null});
        view.status.textContent='Loading view…';view.frame.hidden=true;view.button.hidden=true;
        clearTimeout(view.timer);view.timer=setTimeout(()=>fail(view,new Error('App initialization timed out')),15000);
      }
      if(view && latest.recordId!==view.record) {
        const record=latest.recordId;view.record=record;view.callId=latest.callId;view.tool=latest.tool;view.piToolName=latest.piToolName;heading(view,latest);
        if(isPreview(latest)) {
          // A fresh window keeps late messages from the previous ticket out of this handshake.
          const frame=view.frame.cloneNode(false);frame.removeAttribute('src');frame.hidden=true;
          view.observer.unobserve(view.frame);view.frame.replaceWith(frame);view.frame=frame;view.observer.observe(frame);
          view.ready=false;view.noReuse=config.noReuse.includes(latest.server);
        }
        request(`/chat/${encodeURIComponent(app.slot)}/apps/${encodeURIComponent(record)}`).then(data=>{
          if(view.record!==record || view.destroying)return;
          view.data=data;view.dataRecord=record;
          if(isPreview(latest)) {
            const origin=new URL(data.sandboxOrigin);
            if(!['http:','https:'].includes(origin.protocol) || origin.hostname===location.hostname)throw new Error('App and chat must use separate hostnames. Open local chat at 127.0.0.1.');
            if(view.origin && origin.origin!==view.origin)throw new Error('Invalid sandbox assignment');
            view.origin=origin.origin;
            const url=new URL(data.sandboxUrl);
            if(url.origin!==view.origin)throw new Error('Invalid sandbox assignment');
            Object.assign(view,{bootstrapped:false,initializing:false,ready:false,delivered:null,inputCallId:null});
            view.status.textContent='Loading view…';view.frame.hidden=true;view.button.hidden=true;
            clearTimeout(view.timer);view.timer=setTimeout(()=>fail(view,new Error('App initialization timed out')),15000);
            view.frame.src=url.href;
          } else {
            if(view.ready){view.frame.hidden=false;view.status.textContent=''}deliver(view);
          }
        }).catch(error=>{if(view.record===record && !view.destroying)fail(view,error)});
      }
      for(const d of follows?descriptors.slice(0,-1):descriptors.slice(1)) {
        const target=anchorOf(d);
        if(!target || target.firstChild) continue;
        const link=document.createElement('button');link.type='button';
        if(follows){
          link.className='chat-app-marker';link.dataset.appMarker='';
          link.textContent=`${isPreview(d)?d.previewTitle || new URL(d.previewUrl).hostname:d.artifactTitle || d.artifactFilename || 'Artifact'} · newer version below`;
          link.onclick=()=>{
            if(view.mode==='fullscreen')attention(view);
            else view.section.scrollIntoView({block:'center'});
          };
        } else {link.textContent=`View updated: ${d.server}`;link.onclick=()=>view.section.scrollIntoView({block:'center'})}
        target.append(link);
      }
    }
  };
  const event = (app,message) => {
    if(message.type==='reset'){autoOpen.clear();for(const view of views.values())teardown(view)}
    if(message.type==='tool_output' && message.app?.server==='dar' && !message.historical && (!isPreview(message.app) || message.app.previewFrameable===true))autoOpen.add(message.app.recordId);
    if(message.type==='aborted' || message.type==='closed')for(const view of views.values())if(view.slot===app.slot)notify(view,'ui/notifications/tool-cancelled',{reason:message.error || 'Tool cancelled'});
    if(message.type==='tool_output' && message.done && !message.app) for(const view of views.values())if(view.slot===app.slot && view.inputCallId===message.id) notify(view,'ui/notifications/tool-cancelled',{reason:message.is_error?'Tool failed':'UI result unavailable'});
    if(message.type==='tool_call')for(const view of views.values())if(view.slot===app.slot && view.ready && !view.noReuse && (normalize(message.name)===view.tool || view.piToolName && message.name===view.piToolName)) {
      try{const args=JSON.parse(message.args);if(view.previewUrl?withoutFragment(args.url)!==withoutFragment(view.previewUrl):view.server==='dar' && (args.filename || args.path?.split('/').pop())!==view.artifactFilename)continue;notify(view,'ui/notifications/tool-input',{arguments:args});view.inputCallId=message.id}catch{}
    }
  };
  addEventListener('resize',()=>{if(Array.from(views.values()).some(view=>view.mode==='fullscreen'))layoutPanel()});
  addEventListener('pagehide',()=>{for(const view of views.values())if(view.ready)post(view,{jsonrpc:'2.0',id:'dar-teardown',method:'ui/resource-teardown',params:{reason:'Page closed'}})});
  window.DarMcpApps={sync,event};
})();
