#!/usr/bin/env python3
"""Build API compatibility/failure variants in the pinned Mesa tools image."""
from pathlib import Path
import argparse,hashlib,json,subprocess
parser=argparse.ArgumentParser();parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
out=args.output.resolve();out.mkdir();original=(Path(__file__).resolve().parent/'prepass.cpp').read_text()
variants={
 'dispatch-base':original.replace('vkCmdDispatch(command, 1, 1, 1);','vkCmdDispatchBase(command, 0, 0, 0, 1, 1, 1);'),
 'specialization':original.replace('    VkPipeline compute;', '    const uint32_t ignored_value=7; const VkSpecializationMapEntry entry{777,0,4}; const VkSpecializationInfo specialization{1,&entry,4,&ignored_value}; compute_info.stage.pSpecializationInfo=&specialization;\n    VkPipeline compute;'),
 'dynamic-bindings':original.replace('VK_DESCRIPTOR_TYPE_STORAGE_BUFFER','VK_DESCRIPTOR_TYPE_STORAGE_BUFFER_DYNAMIC').replace('    VkDescriptorSet set;', '    const uint32_t dynamic_offsets[2]{0,0};\n    VkDescriptorSet set;').replace('compute_layout, 0, 1, &set, 0, nullptr','compute_layout, 0, 1, &set, 2, dynamic_offsets'),
 'wide-range':original.replace('Buffer source = buffer(size,','Buffer source = buffer(size + 4,').replace('{{source.buffer, 0, size}, {destination.buffer, guard, size}}','{{source.buffer, 0, size + 4}, {destination.buffer, guard, size + 4}}')}
query=original.replace('    VK(vkCreateDevice(physical,','    VkPhysicalDeviceFeatures enabled{}; enabled.pipelineStatisticsQuery=VK_TRUE; device_info.pEnabledFeatures=&enabled;\n    VK(vkCreateDevice(physical,')
query=query.replace('    uint32_t colored = 0;', '''    VkQueryPoolCreateInfo query_info{VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO}; query_info.queryType=VK_QUERY_TYPE_PIPELINE_STATISTICS; query_info.queryCount=1; query_info.pipelineStatistics=VK_QUERY_PIPELINE_STATISTIC_COMPUTE_SHADER_INVOCATIONS_BIT;
    VkQueryPool query; VK(vkCreateQueryPool(device,&query_info,nullptr,&query));
    uint32_t colored = 0;''')
query=query.replace('vkCmdDispatch(command, 1, 1, 1);','vkCmdResetQueryPool(command,query,0,1); vkCmdBeginQuery(command,query,0,0); vkCmdDispatch(command, 1, 1, 1); vkCmdEndQuery(command,query,0);')
query=query.replace('        for (uint32_t i = 0; i < guard;', '        uint64_t invocations=0; VK(vkGetQueryPoolResults(device,query,0,1,sizeof invocations,&invocations,sizeof invocations,VK_QUERY_RESULT_64_BIT|VK_QUERY_RESULT_WAIT_BIT)); require(invocations==64,"pipeline statistics lost compute invocations");\n        for (uint32_t i = 0; i < guard;')
query=query.replace('    vkDestroyFence(device,','    vkDestroyQueryPool(device,query,nullptr);\n    vkDestroyFence(device,');variants['queries']=query
for mode in ['timeline','timeline-before','queue-idle','device-idle']:
 s=original.replace('#include <vector>','#include <vector>\n#include <thread>\n#include <atomic>\n#include <chrono>').replace('VK_API_VERSION_1_1','VK_API_VERSION_1_2')
 s=s.replace('    VK(vkCreateDevice(physical,','    VkPhysicalDeviceTimelineSemaphoreFeatures timeline_feature{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES}; timeline_feature.timelineSemaphore=VK_TRUE; device_info.pNext=&timeline_feature;\n    VK(vkCreateDevice(physical,')
 s=s.replace('    uint32_t colored = 0;', '''    VkSemaphoreTypeCreateInfo type{VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO}; type.semaphoreType=VK_SEMAPHORE_TYPE_TIMELINE;
    VkSemaphoreCreateInfo sem_info{VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO}; sem_info.pNext=&type;
    VkSemaphore timeline; VK(vkCreateSemaphore(device,&sem_info,nullptr,&timeline)); const uint64_t point=1;
    VkSemaphoreWaitInfo wait{VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO}; wait.semaphoreCount=1; wait.pSemaphores=&timeline; wait.pValues=&point;
    VkTimelineSemaphoreSubmitInfo timeline_submit{VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO}; timeline_submit.signalSemaphoreValueCount=1; timeline_submit.pSignalSemaphoreValues=&point;
    uint32_t colored = 0;''')
 s=s.replace('VkResult submitted = vkQueueSubmit(queue, 1, &submit, fence);','submit.pNext=&timeline_submit; submit.signalSemaphoreCount=1; submit.pSignalSemaphores=&timeline;\n        '+('VkResult waiter_result=VK_NOT_READY; std::atomic<bool> waiter_started{false}; std::thread waiter([&]{waiter_started=true;waiter_result=vkWaitSemaphores(device,&wait,UINT64_MAX);}); while(!waiter_started) std::this_thread::yield(); std::this_thread::sleep_for(std::chrono::milliseconds(50));\n        ' if mode=='timeline-before' else '')+'VkResult submitted = vkQueueSubmit(queue, 1, &submit, fence);')
 old='VkResult waited = submitted == VK_SUCCESS ? vkWaitForFences(device, 1, &fence, VK_TRUE, expect_lost ? UINT64_MAX : 30000000000ull) : submitted;'
 new={'timeline':'VkResult waited = submitted == VK_SUCCESS ? vkWaitSemaphores(device,&wait,UINT64_MAX) : submitted;', 'timeline-before':'waiter.join(); VkResult waited=waiter_result;', 'queue-idle':'VkResult waited = submitted == VK_SUCCESS ? vkQueueWaitIdle(queue) : submitted;', 'device-idle':'VkResult waited = submitted == VK_SUCCESS ? vkDeviceWaitIdle(device) : submitted;'}[mode]
 s=s.replace(old,new);s=s.replace('    const bool expect_lost = argc == 4;','    const bool expect_lost = argc == 4; require(expect_lost,"failure-only qualification");')
 # Avoid unused wait declarations in idle variants, while preserving feature/submit setup.
 if mode.endswith('idle'):s=s.replace('    VkSemaphoreWaitInfo wait{','    [[maybe_unused]] VkSemaphoreWaitInfo wait{')
 variants['bad-reply-'+mode]=s
# Valid DGC with an already-bound pipeline and only a DISPATCH token.
s=original.replace('VK_API_VERSION_1_1','VK_API_VERSION_1_3')
s=s.replace('static Buffer buffer(VkDeviceSize size, VkBufferUsageFlags usage)', 'static Buffer buffer(VkDeviceSize size, VkBufferUsageFlags2KHR usage)')
s=s.replace('info.usage = usage;', 'VkBufferUsageFlags2CreateInfoKHR usage_info{VK_STRUCTURE_TYPE_BUFFER_USAGE_FLAGS_2_CREATE_INFO_KHR}; usage_info.usage=usage; info.pNext=&usage_info;')
s=s.replace('VK(vkAllocateMemory(device, &alloc,', 'VkMemoryAllocateFlagsInfo flags{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO}; flags.flags=VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT; alloc.pNext=&flags;\n    VK(vkAllocateMemory(device, &alloc,')
s=s.replace('    VK(vkCreateDevice(physical,', '''    const char *extensions[]{VK_EXT_DEVICE_GENERATED_COMMANDS_EXTENSION_NAME,VK_KHR_MAINTENANCE_5_EXTENSION_NAME}; device_info.enabledExtensionCount=2; device_info.ppEnabledExtensionNames=extensions;
    VkPhysicalDeviceDeviceGeneratedCommandsFeaturesEXT dgc{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DEVICE_GENERATED_COMMANDS_FEATURES_EXT}; dgc.deviceGeneratedCommands=VK_TRUE;
    VkPhysicalDeviceBufferDeviceAddressFeatures bda{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES}; bda.bufferDeviceAddress=VK_TRUE; bda.pNext=&dgc;
    VkPhysicalDeviceMaintenance5FeaturesKHR maintenance{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAINTENANCE_5_FEATURES_KHR}; maintenance.maintenance5=VK_TRUE; maintenance.pNext=&bda; device_info.pNext=&maintenance;
    VK(vkCreateDevice(physical,''')
s=s.replace('    uint32_t colored = 0;', '''    auto create_layout=reinterpret_cast<PFN_vkCreateIndirectCommandsLayoutEXT>(vkGetDeviceProcAddr(device,"vkCreateIndirectCommandsLayoutEXT"));
    auto destroy_layout=reinterpret_cast<PFN_vkDestroyIndirectCommandsLayoutEXT>(vkGetDeviceProcAddr(device,"vkDestroyIndirectCommandsLayoutEXT"));
    auto memory_requirements=reinterpret_cast<PFN_vkGetGeneratedCommandsMemoryRequirementsEXT>(vkGetDeviceProcAddr(device,"vkGetGeneratedCommandsMemoryRequirementsEXT"));
    auto execute=reinterpret_cast<PFN_vkCmdExecuteGeneratedCommandsEXT>(vkGetDeviceProcAddr(device,"vkCmdExecuteGeneratedCommandsEXT")); require(create_layout&&destroy_layout&&memory_requirements&&execute,"DGC functions unavailable");
    VkIndirectCommandsLayoutTokenEXT token{VK_STRUCTURE_TYPE_INDIRECT_COMMANDS_LAYOUT_TOKEN_EXT}; token.type=VK_INDIRECT_COMMANDS_TOKEN_TYPE_DISPATCH_EXT;
    VkIndirectCommandsLayoutCreateInfoEXT indirect_info{VK_STRUCTURE_TYPE_INDIRECT_COMMANDS_LAYOUT_CREATE_INFO_EXT}; indirect_info.shaderStages=VK_SHADER_STAGE_COMPUTE_BIT; indirect_info.indirectStride=sizeof(VkDispatchIndirectCommand); indirect_info.pipelineLayout=compute_layout; indirect_info.tokenCount=1; indirect_info.pTokens=&token;
    VkIndirectCommandsLayoutEXT indirect_layout; VK(create_layout(device,&indirect_info,nullptr,&indirect_layout));
    VkGeneratedCommandsPipelineInfoEXT pipeline_info{VK_STRUCTURE_TYPE_GENERATED_COMMANDS_PIPELINE_INFO_EXT}; pipeline_info.pipeline=compute;
    VkGeneratedCommandsMemoryRequirementsInfoEXT memory_info{VK_STRUCTURE_TYPE_GENERATED_COMMANDS_MEMORY_REQUIREMENTS_INFO_EXT}; memory_info.pNext=&pipeline_info; memory_info.indirectCommandsLayout=indirect_layout; memory_info.maxSequenceCount=1;
    VkMemoryRequirements2 required{VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2}; memory_requirements(device,&memory_info,&required);
    Buffer indirect=buffer(sizeof(VkDispatchIndirectCommand),VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT|VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
    Buffer preprocess=buffer(required.memoryRequirements.size+required.memoryRequirements.alignment,VK_BUFFER_USAGE_2_PREPROCESS_BUFFER_BIT_EXT|VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT);
    VkMemoryRequirements pre_req; vkGetBufferMemoryRequirements(device,preprocess.buffer,&pre_req); require(required.memoryRequirements.memoryTypeBits&(1u<<memory_type(pre_req.memoryTypeBits,VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT|VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)),"incompatible preprocess memory");
    const VkDispatchIndirectCommand dispatch{1,1,1}; std::memcpy(indirect.mapped,&dispatch,sizeof dispatch);
    VkBufferDeviceAddressInfo address{VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO}; address.buffer=indirect.buffer;
    VkGeneratedCommandsInfoEXT generated{VK_STRUCTURE_TYPE_GENERATED_COMMANDS_INFO_EXT}; generated.pNext=&pipeline_info; generated.shaderStages=VK_SHADER_STAGE_COMPUTE_BIT; generated.indirectCommandsLayout=indirect_layout; generated.indirectAddress=vkGetBufferDeviceAddress(device,&address); generated.indirectAddressSize=sizeof dispatch; generated.maxSequenceCount=1;
    address.buffer=preprocess.buffer; const VkDeviceAddress pre_address=vkGetBufferDeviceAddress(device,&address), alignment=required.memoryRequirements.alignment;
    generated.preprocessAddress=(pre_address+alignment-1)&~(alignment-1); generated.preprocessSize=required.memoryRequirements.size;
    uint32_t colored = 0;''')
s=s.replace('vkCmdDispatch(command, 1, 1, 1);','execute(command,VK_FALSE,&generated);')
s=s.replace('host.dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT;', 'host.dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT | VK_ACCESS_INDIRECT_COMMAND_READ_BIT;').replace('VK_PIPELINE_STAGE_HOST_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,','VK_PIPELINE_STAGE_HOST_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_DRAW_INDIRECT_BIT,')
s=s.replace('    vkDestroyFence(device,','    destroy(indirect); destroy(preprocess); destroy_layout(device,indirect_layout,nullptr);\n    vkDestroyFence(device,');variants['generated']=s
manifest={}
for name,text in variants.items():
 source=out/(name+'.cpp');source.write_text(text)
 subprocess.run(['c++','-std=c++17','-O2','-pthread','-Wall','-Wextra','-Werror','-Wno-missing-field-initializers',str(source),'-lvulkan','-o',str(out/('prepass-'+name))],check=True)
 manifest[name]=hashlib.sha256(source.read_bytes()).hexdigest()
(out/'sources.json').write_text(json.dumps(manifest,sort_keys=True,indent=2)+'\n')
