import {sha,Refusal} from './core.mjs';
export function privateUpdateAction(plan,migration,privateState){
 if(migration?.status!=='verified'||!privateState?.verified)throw new Refusal('Stored encrypted snapshots and the provisioned key must both be verified');
 const cfg=plan.privateSnapshot;if(migration.deployment!==cfg.deploymentId)throw new Refusal('Migration deployment mismatch');
 const boot=migration.objects.find(x=>x.role==='boot'),saved=migration.objects.find(x=>x.role==='saved');
 if(!boot||boot.stage!=='verified'||!saved||(!saved.missing&&saved.stage!=='verified'))throw new Refusal('Migration is incomplete');
 const container=JSON.parse(cfg.baseline.configCid),config=container.config;
 if(boot.source!==config.snapshot||saved.source!==config.snapshotSaveKey)throw new Refusal('Migration source objects differ from the deployment baseline');
 config.snapshot=boot.target;config.snapshotSaveKey=saved.missing?`private/${cfg.deploymentId}/session.snap`:saved.target;
 config.snapshotEncryption={masterKey:'$RISCBOX_SNAPSHOT_KEY',owner:plan.wallet.toLowerCase(),deployment:cfg.deploymentId};
 const app=plan.apps.find(a=>a.slug==='risc-box'),appId=app.catalogAppIds[0],nextAppRef=`catalog://${appId}/70`,nextConfig=JSON.stringify(container);
 return {id:'risc-private-update-070',kind:'update',slug:'risc-box',title:'Select RISC Box 0.6.70 with verified private snapshots',deploymentId:cfg.deploymentId,nextAppRef,nextConfig,migrationSha256:sha(JSON.stringify(migration)),gasCap:'2500000',maxFeePerGas:'11000000',maxPriorityFeePerGas:'1000000',preconditions:[{contract:'deployments',fn:'get',args:[cfg.deploymentId],fields:cfg.baseline},{contract:'appCatalog',fn:'getVersion',args:[appId,70],fields:{version:'0.6.70',cid:app.artifact.cid,yanked:false}}],postconditions:[{contract:'deployments',fn:'get',args:[cfg.deploymentId],fields:{owner:cfg.baseline.owner,appRef:nextAppRef,configCid:nextConfig,active:false}}]};
}
