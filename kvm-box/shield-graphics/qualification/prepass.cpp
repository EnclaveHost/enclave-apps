#ifndef SHIELD_TEST_GLOBAL_BIAS
#define SHIELD_TEST_GLOBAL_BIAS 0
#endif
#ifndef SHIELD_TEST_LOCAL_BIAS
#define SHIELD_TEST_LOCAL_BIAS 0
#endif
#ifndef SHIELD_TEST_GROUPS
#define SHIELD_TEST_GROUPS 1
#endif
static_assert(SHIELD_TEST_GROUPS >= 1 && SHIELD_TEST_GROUPS <= 65, "bounded test dispatch");
// Standard Vulkan qualification: SSBO compute -> vertex buffer -> raster -> readback.
// No Shield socket or physical-GPU API is called by this application.
#include <vulkan/vulkan.h>
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <string>
#include <vector>

static void require(bool ok, const char *what) {
    if (!ok) { std::fprintf(stderr, "%s\n", what); std::exit(1); }
}
static void check(VkResult r, const char *what) {
    if (r != VK_SUCCESS) { std::fprintf(stderr, "%s: VkResult %d\n", what, r); std::exit(1); }
}
#define VK(call) check(call, #call)
static VkDevice device;
static VkPhysicalDeviceMemoryProperties memory_properties;
static uint32_t memory_type(uint32_t mask, VkMemoryPropertyFlags required) {
    for (uint32_t i = 0; i < memory_properties.memoryTypeCount; ++i)
        if ((mask & (1u << i)) && (memory_properties.memoryTypes[i].propertyFlags & required) == required) return i;
    require(false, "no compatible memory type"); return 0;
}
struct Buffer { VkBuffer buffer{}; VkDeviceMemory memory{}; uint8_t *mapped{}; };
static Buffer buffer(VkDeviceSize size, VkBufferUsageFlags usage) {
    Buffer b;
    VkBufferCreateInfo info{VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO}; info.size = size; info.usage = usage;
    VK(vkCreateBuffer(device, &info, nullptr, &b.buffer));
    VkMemoryRequirements req; vkGetBufferMemoryRequirements(device, b.buffer, &req);
    VkMemoryAllocateInfo alloc{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO}; alloc.allocationSize = req.size;
    alloc.memoryTypeIndex = memory_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    VK(vkAllocateMemory(device, &alloc, nullptr, &b.memory)); VK(vkBindBufferMemory(device, b.buffer, b.memory, 0));
    void *map = nullptr; VK(vkMapMemory(device, b.memory, 0, VK_WHOLE_SIZE, 0, &map)); b.mapped = static_cast<uint8_t *>(map);
    return b;
}
static void destroy(Buffer &b) { vkUnmapMemory(device, b.memory); vkDestroyBuffer(device, b.buffer, nullptr); vkFreeMemory(device, b.memory, nullptr); }
static VkShaderModule shader(const std::string &path) {
    std::ifstream input(path, std::ios::binary | std::ios::ate); require(bool(input), "cannot open SPIR-V module");
    auto length = input.tellg(); require(length > 0 && length < (1 << 20) && length % 4 == 0, "invalid SPIR-V length");
    std::vector<uint32_t> words(size_t(length) / 4); input.seekg(0); input.read(reinterpret_cast<char *>(words.data()), length);
    require(bool(input), "cannot read SPIR-V module");
    VkShaderModuleCreateInfo info{VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO}; info.codeSize = size_t(length); info.pCode = words.data();
    VkShaderModule result; VK(vkCreateShaderModule(device, &info, nullptr, &result)); return result;
}
int main(int argc, char **argv) {
    require(argc == 3 || (argc == 4 && !std::strcmp(argv[3], "--expect-device-lost")), "usage: prepass SHADER_DIRECTORY NEW_RGBA_OUTPUT [--expect-device-lost]");
    const bool expect_lost = argc == 4;
    constexpr uint32_t rows = 64 * SHIELD_TEST_GROUPS, dimensions = 4, size = rows * dimensions * 4, guard = 16, width = 128, height = 128;
    VkApplicationInfo app{VK_STRUCTURE_TYPE_APPLICATION_INFO}; app.pApplicationName = "Shield graphics API qualification"; app.apiVersion = VK_API_VERSION_1_1;
    VkInstanceCreateInfo instance_info{VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO}; instance_info.pApplicationInfo = &app;
    VkInstance instance; VK(vkCreateInstance(&instance_info, nullptr, &instance));
    uint32_t count = 0; VK(vkEnumeratePhysicalDevices(instance, &count, nullptr)); require(count == 1, "qualification requires exactly one pinned software ICD device");
    VkPhysicalDevice physical; VK(vkEnumeratePhysicalDevices(instance, &count, &physical));
    VkPhysicalDeviceProperties properties; vkGetPhysicalDeviceProperties(physical, &properties);
    require(properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU, "qualification refuses a raw hardware GPU");
    vkGetPhysicalDeviceMemoryProperties(physical, &memory_properties);
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, nullptr); std::vector<VkQueueFamilyProperties> queues(count);
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, queues.data()); uint32_t family = count;
    for (uint32_t i = 0; i < count; ++i) if ((queues[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) == (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) { family = i; break; }
    require(family < count, "no graphics/compute queue");
    float priority = 1; VkDeviceQueueCreateInfo queue_info{VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO}; queue_info.queueFamilyIndex = family; queue_info.queueCount = 1; queue_info.pQueuePriorities = &priority;
    VkDeviceCreateInfo device_info{VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO}; device_info.queueCreateInfoCount = 1; device_info.pQueueCreateInfos = &queue_info;
    VK(vkCreateDevice(physical, &device_info, nullptr, &device)); VkQueue queue; vkGetDeviceQueue(device, family, 0, &queue);
    Buffer source = buffer(size, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);
    Buffer destination = buffer(size + 2 * guard, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_VERTEX_BUFFER_BIT);
    Buffer pixels = buffer(width * height * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT);
    VkDescriptorSetLayoutBinding bindings[2]{};
    for (uint32_t i = 0; i < 2; ++i) { bindings[i].binding = i; bindings[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER; bindings[i].descriptorCount = 1; bindings[i].stageFlags = VK_SHADER_STAGE_COMPUTE_BIT; }
    VkDescriptorSetLayoutCreateInfo set_info{VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO}; set_info.bindingCount = 2; set_info.pBindings = bindings;
    VkDescriptorSetLayout set_layout; VK(vkCreateDescriptorSetLayout(device, &set_info, nullptr, &set_layout));
    VkPipelineLayoutCreateInfo layout_info{VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO}; layout_info.setLayoutCount = 1; layout_info.pSetLayouts = &set_layout;
    VkPipelineLayout compute_layout; VK(vkCreatePipelineLayout(device, &layout_info, nullptr, &compute_layout));
    layout_info.setLayoutCount = 0; VkPipelineLayout graphics_layout; VK(vkCreatePipelineLayout(device, &layout_info, nullptr, &graphics_layout));
    VkDescriptorPoolSize pool_size{VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 2}; VkDescriptorPoolCreateInfo pool_info{VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO}; pool_info.maxSets = 1; pool_info.poolSizeCount = 1; pool_info.pPoolSizes = &pool_size;
    VkDescriptorPool pool; VK(vkCreateDescriptorPool(device, &pool_info, nullptr, &pool));
    VkDescriptorSetAllocateInfo set_alloc{VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO}; set_alloc.descriptorPool = pool; set_alloc.descriptorSetCount = 1; set_alloc.pSetLayouts = &set_layout;
    VkDescriptorSet set; VK(vkAllocateDescriptorSets(device, &set_alloc, &set));
    VkDescriptorBufferInfo buffers[2]{{source.buffer, 0, size}, {destination.buffer, guard, size}}; VkWriteDescriptorSet writes[2]{};
    for (uint32_t i = 0; i < 2; ++i) { writes[i].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET; writes[i].dstSet = set; writes[i].dstBinding = i; writes[i].descriptorCount = 1; writes[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER; writes[i].pBufferInfo = &buffers[i]; }
    vkUpdateDescriptorSets(device, 2, writes, 0, nullptr);
    const std::string directory = argv[1]; VkShaderModule cs = shader(directory + "/linear-prepass.comp.spv"), vs = shader(directory + "/prepass.vert.spv"), fs = shader(directory + "/prepass.frag.spv");
    VkComputePipelineCreateInfo compute_info{VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO}; compute_info.layout = compute_layout; compute_info.stage.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO; compute_info.stage.stage = VK_SHADER_STAGE_COMPUTE_BIT; compute_info.stage.module = cs; compute_info.stage.pName = "main";
    VkPipeline compute; VK(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &compute_info, nullptr, &compute));
    VkImageCreateInfo image_info{VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO}; image_info.imageType = VK_IMAGE_TYPE_2D; image_info.format = VK_FORMAT_R8G8B8A8_UNORM; image_info.extent = {width, height, 1}; image_info.mipLevels = 1; image_info.arrayLayers = 1; image_info.samples = VK_SAMPLE_COUNT_1_BIT; image_info.tiling = VK_IMAGE_TILING_OPTIMAL; image_info.usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    VkImage image; VK(vkCreateImage(device, &image_info, nullptr, &image)); VkMemoryRequirements req; vkGetImageMemoryRequirements(device, image, &req);
    VkMemoryAllocateInfo image_alloc{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO}; image_alloc.allocationSize = req.size; image_alloc.memoryTypeIndex = memory_type(req.memoryTypeBits, 0);
    VkDeviceMemory image_memory; VK(vkAllocateMemory(device, &image_alloc, nullptr, &image_memory)); VK(vkBindImageMemory(device, image, image_memory, 0));
    VkImageViewCreateInfo view_info{VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO}; view_info.image = image; view_info.viewType = VK_IMAGE_VIEW_TYPE_2D; view_info.format = image_info.format; view_info.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    VkImageView view; VK(vkCreateImageView(device, &view_info, nullptr, &view));
    VkAttachmentDescription attachment{}; attachment.format = image_info.format; attachment.samples = VK_SAMPLE_COUNT_1_BIT; attachment.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR; attachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE; attachment.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED; attachment.finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    VkAttachmentReference color_ref{0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL}; VkSubpassDescription subpass{}; subpass.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS; subpass.colorAttachmentCount = 1; subpass.pColorAttachments = &color_ref;
    VkSubpassDependency dependency{}; dependency.srcSubpass = 0; dependency.dstSubpass = VK_SUBPASS_EXTERNAL; dependency.srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT; dependency.dstStageMask = VK_PIPELINE_STAGE_TRANSFER_BIT; dependency.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT; dependency.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    VkRenderPassCreateInfo pass_info{VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO}; pass_info.attachmentCount = 1; pass_info.pAttachments = &attachment; pass_info.subpassCount = 1; pass_info.pSubpasses = &subpass; pass_info.dependencyCount = 1; pass_info.pDependencies = &dependency;
    VkRenderPass pass; VK(vkCreateRenderPass(device, &pass_info, nullptr, &pass));
    VkFramebufferCreateInfo framebuffer_info{VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO}; framebuffer_info.renderPass = pass; framebuffer_info.attachmentCount = 1; framebuffer_info.pAttachments = &view; framebuffer_info.width = width; framebuffer_info.height = height; framebuffer_info.layers = 1;
    VkFramebuffer framebuffer; VK(vkCreateFramebuffer(device, &framebuffer_info, nullptr, &framebuffer));
    VkPipelineShaderStageCreateInfo stages[2]{};
    for (int i = 0; i < 2; ++i) { stages[i].sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO; stages[i].stage = i ? VK_SHADER_STAGE_FRAGMENT_BIT : VK_SHADER_STAGE_VERTEX_BIT; stages[i].module = i ? fs : vs; stages[i].pName = "main"; }
    VkVertexInputBindingDescription binding{0, 16, VK_VERTEX_INPUT_RATE_VERTEX}; VkVertexInputAttributeDescription attribute{0, 0, VK_FORMAT_R32G32B32A32_SINT, 0};
    VkPipelineVertexInputStateCreateInfo vertex{VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO}; vertex.vertexBindingDescriptionCount = 1; vertex.pVertexBindingDescriptions = &binding; vertex.vertexAttributeDescriptionCount = 1; vertex.pVertexAttributeDescriptions = &attribute;
    VkPipelineInputAssemblyStateCreateInfo assembly{VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO}; assembly.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
    VkViewport viewport{0, 0, float(width), float(height), 0, 1}; VkRect2D scissor{{0, 0}, {width, height}};
    VkPipelineViewportStateCreateInfo viewport_info{VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO}; viewport_info.viewportCount = 1; viewport_info.pViewports = &viewport; viewport_info.scissorCount = 1; viewport_info.pScissors = &scissor;
    VkPipelineRasterizationStateCreateInfo raster{VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO}; raster.polygonMode = VK_POLYGON_MODE_FILL; raster.cullMode = VK_CULL_MODE_NONE; raster.frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE; raster.lineWidth = 1;
    VkPipelineMultisampleStateCreateInfo samples{VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO}; samples.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
    VkPipelineColorBlendAttachmentState blend_attachment{}; blend_attachment.colorWriteMask = 15;
    VkPipelineColorBlendStateCreateInfo blend{VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO}; blend.attachmentCount = 1; blend.pAttachments = &blend_attachment;
    VkGraphicsPipelineCreateInfo graphics_info{VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO}; graphics_info.stageCount = 2; graphics_info.pStages = stages; graphics_info.pVertexInputState = &vertex; graphics_info.pInputAssemblyState = &assembly; graphics_info.pViewportState = &viewport_info; graphics_info.pRasterizationState = &raster; graphics_info.pMultisampleState = &samples; graphics_info.pColorBlendState = &blend; graphics_info.layout = graphics_layout; graphics_info.renderPass = pass;
    VkPipeline graphics; VK(vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &graphics_info, nullptr, &graphics));
    VkCommandPoolCreateInfo command_pool_info{VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO}; command_pool_info.queueFamilyIndex = family; command_pool_info.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    VkCommandPool commands; VK(vkCreateCommandPool(device, &command_pool_info, nullptr, &commands));
    VkCommandBufferAllocateInfo command_alloc{VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO}; command_alloc.commandPool = commands; command_alloc.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY; command_alloc.commandBufferCount = 1;
    VkCommandBuffer command; VK(vkAllocateCommandBuffers(device, &command_alloc, &command));
    VkFenceCreateInfo fence_info{VK_STRUCTURE_TYPE_FENCE_CREATE_INFO}; VkFence fence; VK(vkCreateFence(device, &fence_info, nullptr, &fence));
    uint32_t colored = 0;
    for (uint32_t test = 0; test < 8; ++test) {
        std::array<uint32_t, rows * dimensions> input{}; uint32_t rng = 0x9e3779b9u + test;
        for (auto &value : input) { rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5; value = rng; }
        const uint32_t triangle[12]{88, 88, 128, 192, 168, 88, 128, 192, 128, 168, 128, 192};
        std::memcpy(input.data(), triangle, sizeof triangle);
        // Explicit extrema and high-bit values test masking and sign conversion.
        input[12] = 0; input[13] = 255; input[14] = 0xffffffffu; input[15] = 0xffffff00u;
        std::memcpy(source.mapped, input.data(), size); std::memset(destination.mapped, 0xa5, size + 2 * guard); std::memset(pixels.mapped, 0xcc, width * height * 4);
        VK(vkResetCommandBuffer(command, 0)); VkCommandBufferBeginInfo begin{VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO}; VK(vkBeginCommandBuffer(command, &begin));
        VkMemoryBarrier host{VK_STRUCTURE_TYPE_MEMORY_BARRIER}; host.srcAccessMask = VK_ACCESS_HOST_WRITE_BIT; host.dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT;
        vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_HOST_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 1, &host, 0, nullptr, 0, nullptr);
        vkCmdBindPipeline(command, VK_PIPELINE_BIND_POINT_COMPUTE, compute); vkCmdBindDescriptorSets(command, VK_PIPELINE_BIND_POINT_COMPUTE, compute_layout, 0, 1, &set, 0, nullptr); vkCmdDispatch(command, SHIELD_TEST_GROUPS, 1, 1);
        VkMemoryBarrier computed{VK_STRUCTURE_TYPE_MEMORY_BARRIER}; computed.srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT; computed.dstAccessMask = VK_ACCESS_VERTEX_ATTRIBUTE_READ_BIT;
        vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_VERTEX_INPUT_BIT, 0, 1, &computed, 0, nullptr, 0, nullptr);
        VkClearValue clear{}; clear.color.float32[3] = 1;
        VkRenderPassBeginInfo render{VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO}; render.renderPass = pass; render.framebuffer = framebuffer; render.renderArea = scissor; render.clearValueCount = 1; render.pClearValues = &clear;
        vkCmdBeginRenderPass(command, &render, VK_SUBPASS_CONTENTS_INLINE); vkCmdBindPipeline(command, VK_PIPELINE_BIND_POINT_GRAPHICS, graphics); VkDeviceSize offset = guard; vkCmdBindVertexBuffers(command, 0, 1, &destination.buffer, &offset); vkCmdDraw(command, 3, 1, 0, 0); vkCmdEndRenderPass(command);
        VkBufferImageCopy copy{}; copy.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1}; copy.imageExtent = {width, height, 1}; vkCmdCopyImageToBuffer(command, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, pixels.buffer, 1, &copy);
        VkMemoryBarrier readback{VK_STRUCTURE_TYPE_MEMORY_BARRIER}; readback.srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT | VK_ACCESS_TRANSFER_WRITE_BIT; readback.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
        vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &readback, 0, nullptr, 0, nullptr);
        VK(vkEndCommandBuffer(command)); VkSubmitInfo submit{VK_STRUCTURE_TYPE_SUBMIT_INFO}; submit.commandBufferCount = 1; submit.pCommandBuffers = &command; VkResult submitted = vkQueueSubmit(queue, 1, &submit, fence);
        VkResult waited = submitted == VK_SUCCESS ? vkWaitForFences(device, 1, &fence, VK_TRUE, expect_lost ? UINT64_MAX : 30000000000ull) : submitted;
        if (expect_lost) {
            require(waited == VK_ERROR_DEVICE_LOST, "expected visible Vulkan device loss");
            for (uint32_t i = 0; i < size + 2 * guard; ++i) require(destination.mapped[i] == 0xa5, "failed protected dispatch published output");
            for (uint32_t i = 0; i < width * height * 4; ++i) require(pixels.mapped[i] == 0xcc, "draw/readback executed after protected failure");
            require(std::memcmp(source.mapped, input.data(), size) == 0, "failed dispatch changed input");
            std::puts("{\"passed\":true,\"deviceLost\":true,\"outputUnchanged\":true,\"laterDrawSkipped\":true}");
            return 0; // Test process exits with its lost device; no reuse/retry.
        }
        check(submitted, "vkQueueSubmit"); check(waited, "vkWaitForFences"); VK(vkResetFences(device, 1, &fence));
        for (uint32_t i = 0; i < guard; ++i) require(destination.mapped[i] == 0xa5 && destination.mapped[guard + size + i] == 0xa5, "compute overwrote a guard");
        for (uint32_t row = 0; row < rows; ++row) {
            int32_t x[4], actual[4]; for (int j = 0; j < 4; ++j) x[j] = int32_t(input[4 * row + j] & 255u) - 128;
            const int32_t expected[4]{x[0] + x[1] + int32_t(SHIELD_TEST_LOCAL_BIAS*(row%64)+SHIELD_TEST_GLOBAL_BIAS*row), x[1] - x[0], x[2], x[3]}; std::memcpy(actual, destination.mapped + guard + 16 * row, 16);
            require(std::memcmp(expected, actual, 16) == 0, "shader differs from independent integer reference");
        }
        require(std::memcmp(source.mapped, input.data(), size) == 0, "compute modified readonly input");
        colored = 0; for (uint32_t i = 0; i < width * height; ++i) if (pixels.mapped[4 * i] || pixels.mapped[4 * i + 1] || pixels.mapped[4 * i + 2]) ++colored;
        require(colored > 1000 && colored < width * height / 2, "unexpected triangle coverage");
    }
    // Exclusive creation avoids overwriting any existing evidence file.
    FILE *output = std::fopen(argv[2], "wbx"); require(output, "output already exists or cannot be created");
    require(std::fwrite(pixels.mapped, 1, width * height * 4, output) == width * height * 4, "frame write failed"); require(std::fclose(output) == 0, "frame close failed");
    std::printf("{\"passed\":true,\"deviceType\":\"CPU\",\"shaderOutputsChecked\":%u,\"dispatches\":8,\"guardsIntact\":true,\"width\":128,\"height\":128,\"coloredPixels\":%u,\"hardwareAccelerationClaim\":false}\n", rows*4*8, colored);
    vkDestroyFence(device, fence, nullptr); vkDestroyCommandPool(device, commands, nullptr); vkDestroyPipeline(device, graphics, nullptr); vkDestroyFramebuffer(device, framebuffer, nullptr); vkDestroyRenderPass(device, pass, nullptr); vkDestroyImageView(device, view, nullptr); vkDestroyImage(device, image, nullptr); vkFreeMemory(device, image_memory, nullptr); vkDestroyPipeline(device, compute, nullptr);
    for (auto module : {cs, vs, fs}) vkDestroyShaderModule(device, module, nullptr);
    vkDestroyDescriptorPool(device, pool, nullptr); vkDestroyPipelineLayout(device, compute_layout, nullptr); vkDestroyPipelineLayout(device, graphics_layout, nullptr); vkDestroyDescriptorSetLayout(device, set_layout, nullptr); destroy(source); destroy(destination); destroy(pixels); vkDestroyDevice(device, nullptr); vkDestroyInstance(instance, nullptr);
}
