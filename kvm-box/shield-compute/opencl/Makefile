CXXFLAGS ?= -O2 -g -std=c++20 -fPIC -Wall -Wextra -Werror
OUT ?= runtime
all: $(OUT)/libOpenCL.so.1
$(OUT)/libOpenCL-shield-cpu.so.1:
	mkdir -p $(OUT)
	cp -L /usr/lib/x86_64-linux-gnu/libOpenCL.so.1 $@
	patchelf --set-soname libOpenCL-shield-cpu.so.1 $@
$(OUT)/libOpenCL.so.1: shield_opencl.cpp opencl.map $(wildcard q8_capture.inc q8_masked.inc source_declines.hpp) $(OUT)/libOpenCL-shield-cpu.so.1
	$(CXX) $(CXXFLAGS) $(CPPFLAGS) -shared $< -L$(OUT) -Wl,--no-as-needed -l:libOpenCL-shield-cpu.so.1 -Wl,--as-needed -Wl,--version-script=opencl.map -Wl,-soname,libOpenCL.so.1,-rpath,'$$ORIGIN',-z,relro,-z,now,-z,noexecstack -ldl -pthread $(LDLIBS) -o $@
	ln -sf libOpenCL.so.1 $(OUT)/libOpenCL.so
clean:
	rm -rf $(OUT) test-opencl
test-opencl: test_opencl.cpp
	$(CXX) -O2 -std=c++20 -Wall -Wextra -Werror $(CPPFLAGS) $< -lOpenCL -o $@
test-event-errors: test_event_errors.cpp
	$(CXX) -O2 -std=c++20 -Wall -Wextra -Werror $< -lOpenCL -pthread -o $@
test-svm: test_svm.cpp
	$(CXX) -O2 -std=c++20 -Wall -Wextra -Werror $(CPPFLAGS) $< -lOpenCL -pthread -o $@
ir-compiler: ir_compiler.cpp
	clang++-18 $$(llvm-config-18 --cxxflags) -O2 -std=c++17 $< $$(llvm-config-18 --ldflags --system-libs --libs core irreader passes transformutils) -o $@
