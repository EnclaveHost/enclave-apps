// An independently selected Enclave deployment id, never learned from host state.
// HOST_DATA is a host-chosen label attested by SNP, not owner authorization.
import {fileURLToPath} from 'node:url';
import {realpathSync} from 'node:fs';
export function deploymentBinding(args) {
 const ids=args.flatMap((value,index)=>value==='--deployment-id'?[args[index+1]]:value.startsWith('--deployment-id=')?[value.slice('--deployment-id='.length)]:[]);
 const dev=args.filter(value=>value==='--development-unbound').length;
 if(args.some(value=>value.startsWith('--development-unbound=')))throw Error('--development-unbound does not take a value');
 if(ids.length>1||dev>1||ids.length+dev!==1)throw Error('Choose exactly one --deployment-id or --development-unbound');
 if(dev)return Object.freeze({mode:'development-unbound'});
 const id=ids[0];
 if(typeof id!=='string'||!/^0x[0-9a-fA-F]{64}$/.test(id)||/^0x0{64}$/.test(id))throw Error('Deployment id must be a nonzero 0x-prefixed bytes32');
 return Object.freeze({mode:'deployment-id',deploymentId:id.toLowerCase(),hostData:id.slice(2).toLowerCase()});
}

// Also used by start.sh before it starts either service, so malformed binding
// arguments cannot leave a broker running after connector startup fails.
if(process.argv[1]&&realpathSync(fileURLToPath(import.meta.url))===realpathSync(process.argv[1])){
 try{deploymentBinding(process.argv.slice(2))}catch(error){console.error(error.message);process.exitCode=2}
}
