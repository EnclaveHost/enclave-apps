import test from 'node:test';
import assert from 'node:assert/strict';
import {allowedRequest,desktopPath,pinnedIdentity} from './native-local-desktop.mjs';
const origin='http://127.0.0.1:12345',token='test-cookie';
const req={headers:{host:'127.0.0.1:12345',origin,cookie:'native_desktop='+token}};
test('local viewer requires session cookie and exact origin/host',()=>{
 assert.equal(allowedRequest(req,origin,token),true);
 for(const update of [{cookie:''},{cookie:'native_desktop=test-cookie-extra'},{host:'evil.invalid'},{origin:'https://evil.invalid'},{'sec-fetch-site':'cross-site'}])assert.equal(allowedRequest({headers:{...req.headers,...update}},origin,token),false);
});
test('only desktop assets are allowed; no automation/admin proxy',()=>{
 for(const u of ['/desktop/vnc.html','/desktop/app/ui.js','/desktop/core/rfb.js'])assert.equal(desktopPath(u),true,u);
 for(const u of ['/exec','/api/status','http://evil.invalid/desktop/vnc.html','/desktop/../exec','/desktop/%2e%2e/exec','/desktop/a%2fb','/desktop/\\../exec','/desktop/a\r\nx'])assert.equal(desktopPath(u),false,u);
});
test('certificate identity errors are not overridden by key pin',()=>{
 const e=pinnedIdentity('expected.invalid',{subject:{CN:'wrong.invalid'},subjectaltname:'DNS:wrong.invalid'},'0'.repeat(64));
 assert.ok(e);assert.equal(e.code,'ERR_TLS_CERT_ALTNAME_INVALID');
});
