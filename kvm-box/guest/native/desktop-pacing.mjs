// Bounded incremental polling for x11vnc, which does not negotiate noVNC's
// ContinuousUpdates extension. Encrypted transport, authentication, encodings,
// full refreshes and display decoding remain under noVNC's control.
export function installPacing(rfb, send, {
    now=()=>performance.now(), hidden=()=>document.hidden,
    schedule=setInterval, cancel=clearInterval,
}={}) {
    const original=rfb._framebufferUpdate;
    const stats={started:now(),lastFrame:now(),frames:0,extraRequests:0,skipped:0};
    let lastSent=-Infinity, credits=3, stopped=false;
    function frame(...args) {
        const done=original.apply(this,args);
        if(done){stats.lastFrame=now();stats.frames++;credits=Math.min(3,credits+1);}
        return done;
    }
    rfb._framebufferUpdate=frame;
    function tick() {
        if(rfb._rfbConnectionState!=='connected'||rfb._enabledContinuousUpdates){stop();return;}
        const t=now();
        // Credits bound extra requests between completed framebuffer updates;
        // no timer-only request accumulation when the connection stalls.
        if(!credits || hidden() || rfb._flushing || rfb._display.pending() ||
           rfb._sock._rQlen-rfb._sock._rQi>262144 ||
           (t-stats.lastFrame>1500 && t-lastSent<1000)) {stats.skipped++;return;}
        send(rfb._sock,true,0,0,rfb._fbWidth,rfb._fbHeight);
        lastSent=t;credits--;stats.extraRequests++;
    }
    const timer=schedule(tick,100);
    function stop(){
        if(stopped)return;stopped=true;cancel(timer);
        if(rfb._framebufferUpdate===frame)rfb._framebufferUpdate=original;
    }
    return {stats,stop,tick};
}
