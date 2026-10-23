(function () {
  'use strict';
  if (window.DarMcpApps) return;
  const views = new Map();
  const normalize = name => { if(['dar.artifact_publish','dar_artifact_publish'].includes(name))return 'artifact_publish';const dot=typeof name==='string'?name.indexOf('.'):-1;return dot>=0 && name.slice(dot+1).includes('__')?name.slice(dot+1):name; };
  let settings;
  const request = async (url, options = {}) => {
    const response = await fetch(url, { ...options, signal: AbortSignal.timeout(32000) });
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
  };
  const audit = (view, method, params) => request(`/chat/${encodeURIComponent(view.slot)}/apps/${encodeURIComponent(view.record)}/rpc`, {
    method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify({jsonrpc:'2.0',id:1,method,params})
  });
  const ICONS={
    inline:'<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/></svg>',
    download:'<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3v12M7 10l5 5 5-5M5 21h14"/></svg>',
    fullscreen:'<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7"/></svg>'};
  const heading = (view,descriptor) => {
    if(view.server!=='dar')return;
    const title=descriptor.artifactTitle || descriptor.artifactFilename || 'Artifact';
    view.name.textContent=title;view.frame.title=title;
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
  const mode = (view, wanted) => {
    if (!['inline','fullscreen'].includes(wanted) || !view.modes.includes(wanted)) return view.mode;
    if (wanted==='fullscreen') {
      for (const other of views.values()) if (other!==view && other.mode==='fullscreen') mode(other,'inline');
    }
    view.mode = wanted;
    view.section.classList.toggle('is-expanded', wanted==='fullscreen');
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
    const download=descriptor.server==='dar'?document.createElement('a'):null;
    const actions=document.createElement('div');actions.className='chat-app-actions';
    if(download){download.className='chat-app-download';download.download='';download.innerHTML=ICONS.download;download.title='Download';download.setAttribute('aria-label','Download');actions.append(download);}
    actions.append(button);header.append(name,actions);
    const status=document.createElement('p');status.setAttribute('role','status');status.textContent='Loading view…';
    const frame=document.createElement('iframe');frame.title=`${descriptor.server} app`;frame.setAttribute('sandbox','allow-scripts allow-same-origin');frame.style.height='300px';
    section.append(header,status,frame);anchor.append(section);Object.assign(view,{section,frame,button,status,name,download,tool:descriptor.tool});heading(view,descriptor);label(view);
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
    const divider=document.createElement('div');divider.className='chat-app-resizer';divider.setAttribute('role','separator');divider.setAttribute('aria-label','Resize app panel');divider.setAttribute('aria-orientation','vertical');section.append(divider);
    divider.onpointerdown=event=>{
      divider.setPointerCapture(event.pointerId);
      divider.onpointermove=event=>{const root=document.getElementById('chat-root');root?.style.setProperty('--app-panel-width',`${Math.max(320,Math.min(innerWidth-400,innerWidth-event.clientX))}px`)};
      divider.onpointerup=()=>{divider.onpointermove=null};
    };
    try {
      const [config,data]=await Promise.all([configuration(),request(`/chat/${encodeURIComponent(app.slot)}/apps/${encodeURIComponent(descriptor.recordId)}`)]);
      if(view.destroying)return;
      if(config.sandboxPort==null)throw new Error('Sandbox listener unavailable');
      const origin=new URL(data.sandboxOrigin);
      if(!['http:','https:'].includes(origin.protocol) || origin.hostname===location.hostname)throw new Error('App and chat must use separate hostnames. Open local chat at 127.0.0.1.');
      view.origin=origin.origin;view.noReuse=config.noReuse.includes(descriptor.server);
      if(view.record===descriptor.recordId || !view.data) {view.data=data;view.dataRecord=descriptor.recordId;}
      const url=new URL(data.sandboxUrl);
      if(url.origin!==origin.origin)throw new Error('Invalid sandbox assignment');
      frame.src=url.href;
      view.timer=setTimeout(()=>fail(view,new Error('App initialization timed out')),15000);
    }catch(error){fail(view,error)}
    return view;
  };
  const sync = async app => {
    const config=await configuration().catch(()=>null);if(!config)return;
    const groups=new Map();
    for(const block of app.blocks) if(block.kind==='tool' && block.app) {
      const d=block.app,key=d.server==='dar'?JSON.stringify([app.slot,d.server,d.resourceUri,d.artifactFilename,...(config.noReuse.includes(d.server)?[d.recordId]:[])]):`${app.slot}:${d.server}:${d.resourceUri}${config.noReuse.includes(d.server)?':'+d.recordId:''}`;
      if(!groups.has(key))groups.set(key,[]);groups.get(key).push(d);
    }
    for(const view of views.values())if(view.slot!==app.slot || !groups.has(view.key))teardown(view);
    for(const [key,descriptors] of groups) {
      const first=descriptors[0],latest=descriptors.at(-1),anchor=document.querySelector(`[data-app-anchor="${CSS.escape(first.recordId)}"]`);
      if(!anchor)continue;
      let view=views.get(key);
      if(!view) {create(app,latest,anchor,key);view=views.get(key)}
      if(view && latest.recordId!==view.record) {
        const record=latest.recordId;view.record=record;view.callId=latest.callId;view.tool=latest.tool;view.piToolName=latest.piToolName;heading(view,latest);
        request(`/chat/${encodeURIComponent(app.slot)}/apps/${encodeURIComponent(record)}`).then(data=>{if(view.record===record && !view.destroying){view.data=data;view.dataRecord=record;if(view.ready){view.frame.hidden=false;view.status.textContent=''}deliver(view)}}).catch(error=>{if(view.record===record && !view.destroying)fail(view,error)});
      }
      for(const d of descriptors.slice(1)) {
        const target=document.querySelector(`[data-app-anchor="${CSS.escape(d.recordId)}"]`);
        if(target && !target.firstChild) {const link=document.createElement('button');link.type='button';link.textContent=`View updated: ${d.server}`;link.onclick=()=>view.section.scrollIntoView({block:'center'});target.append(link)}
      }
    }
  };
  const event = (app,message) => {
    if(message.type==='reset')for(const view of views.values())teardown(view);
    if(message.type==='aborted' || message.type==='closed')for(const view of views.values())if(view.slot===app.slot)notify(view,'ui/notifications/tool-cancelled',{reason:message.error || 'Tool cancelled'});
    if(message.type==='tool_output' && message.done && !message.app) for(const view of views.values())if(view.slot===app.slot && view.inputCallId===message.id) notify(view,'ui/notifications/tool-cancelled',{reason:message.is_error?'Tool failed':'UI result unavailable'});
    if(message.type==='tool_call')for(const view of views.values())if(view.slot===app.slot && view.ready && !view.noReuse && (normalize(message.name)===view.tool || view.piToolName && message.name===view.piToolName)) {
      try{const args=JSON.parse(message.args);if(view.server==='dar' && (args.filename || args.path?.split('/').pop())!==view.artifactFilename)continue;notify(view,'ui/notifications/tool-input',{arguments:args});view.inputCallId=message.id}catch{}
    }
  };
  addEventListener('resize',()=>{if(Array.from(views.values()).some(view=>view.mode==='fullscreen'))layoutPanel()});
  addEventListener('pagehide',()=>{for(const view of views.values())if(view.ready)post(view,{jsonrpc:'2.0',id:'dar-teardown',method:'ui/resource-teardown',params:{reason:'Page closed'}})});
  window.DarMcpApps={sync,event};
})();
