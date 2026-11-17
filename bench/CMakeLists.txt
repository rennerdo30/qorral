cmake_minimum_required(VERSION 3.20)
project(qorral_bench VERSION 2.0.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
  set(CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)
endif()

option(QB_VULKAN "Vulkan backend" ON)
option(QB_OPENGL "OpenGL backend" ON)
if(WIN32)
  option(QB_D3D "Direct3D 9/10/11/12 backends" ON)
else()
  set(QB_D3D OFF)
endif()

if(QB_VULKAN)
  find_package(Vulkan REQUIRED)
endif()

# SDL2: the CMake package first (SDL2 or sdl2-compat), pkg-config as a fallback.
find_package(SDL2 CONFIG QUIET)
if(TARGET SDL2::SDL2)
  set(BENCH_SDL_TARGET SDL2::SDL2)
else()
  find_package(PkgConfig REQUIRED)
  pkg_check_modules(SDL2 REQUIRED IMPORTED_TARGET sdl2)
  set(BENCH_SDL_TARGET PkgConfig::SDL2)
endif()

# Shader tools: glslc (shaderc) or glslangValidator for SPIR-V; spirv-cross for GLSL and HLSL.
find_program(GLSLC_EXECUTABLE glslc HINTS "$ENV{VULKAN_SDK}/bin")
find_program(GLSLANG_EXECUTABLE glslangValidator HINTS "$ENV{VULKAN_SDK}/bin")
find_program(SPIRV_CROSS_EXECUTABLE spirv-cross HINTS "$ENV{VULKAN_SDK}/bin")
if(NOT GLSLC_EXECUTABLE AND NOT GLSLANG_EXECUTABLE)
  message(FATAL_ERROR "Neither glslc nor glslangValidator was found (install shaderc or glslang)")
endif()
if(NOT SPIRV_CROSS_EXECUTABLE)
  message(FATAL_ERROR "spirv-cross was not found (install SPIRV-Cross)")
endif()

set(SHADER_DIR ${CMAKE_CURRENT_SOURCE_DIR}/shaders)
set(GEN ${CMAKE_CURRENT_BINARY_DIR}/shaders_gen)
file(MAKE_DIRECTORY ${GEN})

# name | needs shader model 5 (storage buffers, compute): no SM 4.0 output for those
set(SHADERS
  fullscreen.vert raymarch.frag instanced.vert instanced.frag
  particles.comp particles.vert particles.frag particles_tex.vert particles_sim.frag
  composite.frag bloom_down.frag bloom_up.frag post_tonemap.frag fxaa.frag solid.frag)
set(SM5_ONLY particles.comp particles.vert)
set(SM3_AUTO instanced.vert instanced.frag particles.frag composite.frag bloom_down.frag bloom_up.frag
    post_tonemap.frag fxaa.frag raymarch.frag solid.frag)
file(GLOB SHADER_INCLUDES ${SHADER_DIR}/*.glsl)

set(EMBED_SPIRV ${CMAKE_CURRENT_SOURCE_DIR}/cmake/embed_spirv.cmake)
set(EMBED_TEXT ${CMAKE_CURRENT_SOURCE_DIR}/cmake/embed_text.cmake)

function(glsl_to_spirv in out optimize)
  if(GLSLC_EXECUTABLE)
    set(opt "")
    if(optimize)
      set(opt -O)
    endif()
    set(cmd ${GLSLC_EXECUTABLE} --target-env=vulkan1.2 ${opt} -I ${SHADER_DIR} -o ${out} ${in})
  else()
    set(cmd ${GLSLANG_EXECUTABLE} -V --target-env vulkan1.2 -I${SHADER_DIR} -o ${out} ${in})
  endif()
  set(SPV_CMD ${cmd} PARENT_SCOPE)
endfunction()

set(SHADER_HEADERS)
set(TABLE_INCLUDES "")
set(TABLE_ROWS "")
foreach(shader ${SHADERS})
  string(REPLACE "." "_" var ${shader})
  set(src ${SHADER_DIR}/${shader})
  set(deps ${src} ${SHADER_INCLUDES} ${EMBED_SPIRV} ${EMBED_TEXT})

  # Vulkan: optimized SPIR-V.
  glsl_to_spirv(${src} ${GEN}/${shader}.spv TRUE)
  add_custom_command(OUTPUT ${GEN}/${var}_spv.h
    COMMAND ${SPV_CMD}
    COMMAND ${CMAKE_COMMAND} -DIN=${GEN}/${shader}.spv -DOUT=${GEN}/${var}_spv.h -DVAR=${var}_spv -P ${EMBED_SPIRV}
    DEPENDS ${deps} COMMENT "SPIR-V ${shader}" VERBATIM)

  # Cross-compilation input: unoptimized SPIR-V keeps the names (block "Push", uniforms).
  glsl_to_spirv(${src} ${GEN}/${shader}.x.spv FALSE)
  set(flip "")
  if(shader MATCHES "\\.vert$")
    set(flip --flip-vert-y)  # Direct3D clip space has y up
  endif()
  set(outs ${GEN}/${var}_glsl.h ${GEN}/${var}_hlsl5.h)
  set(cmds
    COMMAND ${SPV_CMD}
    COMMAND ${SPIRV_CROSS_EXECUTABLE} ${GEN}/${shader}.x.spv --version 430 --no-es --glsl-emit-push-constant-as-ubo
            --output ${GEN}/${shader}.glsl
    COMMAND ${CMAKE_COMMAND} -DIN=${GEN}/${shader}.glsl -DOUT=${GEN}/${var}_glsl.h -DVAR=${var}_glsl -P ${EMBED_TEXT}
    COMMAND ${SPIRV_CROSS_EXECUTABLE} ${GEN}/${shader}.x.spv --hlsl --shader-model 50 ${flip}
            --output ${GEN}/${shader}.hlsl5
    COMMAND ${CMAKE_COMMAND} -DIN=${GEN}/${shader}.hlsl5 -DOUT=${GEN}/${var}_hlsl5.h -DVAR=${var}_hlsl5 -P ${EMBED_TEXT})
  # OpenGL: vertex shaders of passes into the window get y flipped (GL shows row 0 at the bottom).
  set(glslflip "nullptr")
  if(shader MATCHES "\\.vert$")
    list(APPEND outs ${GEN}/${var}_glslflip.h)
    list(APPEND cmds
      COMMAND ${SPIRV_CROSS_EXECUTABLE} ${GEN}/${shader}.x.spv --version 430 --no-es --glsl-emit-push-constant-as-ubo
              --flip-vert-y --output ${GEN}/${shader}.flip.glsl
      COMMAND ${CMAKE_COMMAND} -DIN=${GEN}/${shader}.flip.glsl -DOUT=${GEN}/${var}_glslflip.h -DVAR=${var}_glslflip -P ${EMBED_TEXT})
    set(glslflip "${var}_glslflip")
    string(APPEND TABLE_INCLUDES "#include \"${var}_glslflip.h\"\n")
  endif()
  set(hlsl4 "nullptr")
  if(NOT shader IN_LIST SM5_ONLY)
    list(APPEND outs ${GEN}/${var}_hlsl4.h)
    list(APPEND cmds
      COMMAND ${SPIRV_CROSS_EXECUTABLE} ${GEN}/${shader}.x.spv --hlsl --shader-model 40 ${flip}
              --output ${GEN}/${shader}.hlsl4
      COMMAND ${CMAKE_COMMAND} -DIN=${GEN}/${shader}.hlsl4 -DOUT=${GEN}/${var}_hlsl4.h -DVAR=${var}_hlsl4 -P ${EMBED_TEXT})
    set(hlsl4 "${var}_hlsl4")
    string(APPEND TABLE_INCLUDES "#include \"${var}_hlsl4.h\"\n")
  endif()
  add_custom_command(OUTPUT ${outs} ${cmds} DEPENDS ${deps} COMMENT "GLSL/HLSL ${shader}" VERBATIM)

  # Direct3D 9, shader model 3.0: hand-written where SM 3.0 lacks a feature the shader uses (vertex
  # index, texelFetch, integer math), SPIRV-Cross output for the rest.
  set(hlsl3 "nullptr")
  if(EXISTS ${SHADER_DIR}/d3d9/${shader}.hlsl)
    add_custom_command(OUTPUT ${GEN}/${var}_hlsl3.h
      COMMAND ${CMAKE_COMMAND} -DIN=${SHADER_DIR}/d3d9/${shader}.hlsl -DOUT=${GEN}/${var}_hlsl3.h -DVAR=${var}_hlsl3 -P ${EMBED_TEXT}
      DEPENDS ${SHADER_DIR}/d3d9/${shader}.hlsl ${EMBED_TEXT} COMMENT "SM3 ${shader} (hand-written)" VERBATIM)
    set(hlsl3 "${var}_hlsl3")
  elseif(shader IN_LIST SM3_AUTO)
    add_custom_command(OUTPUT ${GEN}/${var}_hlsl3.h
      COMMAND ${SPV_CMD}
      COMMAND ${SPIRV_CROSS_EXECUTABLE} ${GEN}/${shader}.x.spv --hlsl --shader-model 30 ${flip} --output ${GEN}/${shader}.hlsl3
      COMMAND ${CMAKE_COMMAND} -DIN=${GEN}/${shader}.hlsl3 -DOUT=${GEN}/${var}_hlsl3.h -DVAR=${var}_hlsl3 -P ${EMBED_TEXT}
      DEPENDS ${deps} COMMENT "SM3 ${shader}" VERBATIM)
    set(hlsl3 "${var}_hlsl3")
  endif()
  if(NOT hlsl3 STREQUAL "nullptr")
    list(APPEND SHADER_HEADERS ${GEN}/${var}_hlsl3.h)
    string(APPEND TABLE_INCLUDES "#include \"${var}_hlsl3.h\"\n")
  endif()

  list(APPEND SHADER_HEADERS ${GEN}/${var}_spv.h ${outs})
  string(APPEND TABLE_INCLUDES "#include \"${var}_spv.h\"\n#include \"${var}_glsl.h\"\n#include \"${var}_hlsl5.h\"\n")
  string(APPEND TABLE_ROWS "    {\"${shader}\", ${var}_spv, sizeof ${var}_spv, ${var}_glsl, ${glslflip}, ${var}_hlsl5, ${hlsl4}, ${hlsl3}},\n")
endforeach()

set(TABLE_CPP ${GEN}/shader_table.cpp)
file(CONFIGURE OUTPUT ${TABLE_CPP} CONTENT "// Generated by CMakeLists.txt; do not edit.
#include \"gfx/gfx.h\"
@TABLE_INCLUDES@
namespace gfx {
extern const ShaderSource kShaders[];
extern const size_t kShaderCount;
const ShaderSource kShaders[] = {
@TABLE_ROWS@};
const size_t kShaderCount = sizeof kShaders / sizeof kShaders[0];
}  // namespace gfx
" @ONLY)

set(SOURCES
  src/main.cpp
  src/gfx/gfx.h
  src/gfx/common.cpp
  src/scenes/scenes.h
  src/scenes/raymarch.cpp
  src/scenes/instanced.cpp
  src/scenes/particles.cpp
  src/scenes/postfx.cpp
  src/scenes/latency.cpp
  ${TABLE_CPP}
  ${SHADER_HEADERS})
set(DEFS SDL_MAIN_HANDLED QB_VERSION="${PROJECT_VERSION}")
set(LIBS ${BENCH_SDL_TARGET})
if(QB_VULKAN)
  list(APPEND SOURCES src/gfx/vulkan.cpp)
  list(APPEND DEFS QB_HAVE_VULKAN)
  list(APPEND LIBS Vulkan::Vulkan)
endif()
if(QB_OPENGL)
  list(APPEND SOURCES src/gfx/opengl.cpp)
  list(APPEND DEFS QB_HAVE_OPENGL)
endif()
if(QB_D3D)
  list(APPEND SOURCES src/gfx/d3d_common.h src/gfx/d3d_common.cpp src/gfx/d3d9.cpp src/gfx/d3d10.cpp
       src/gfx/d3d11.cpp src/gfx/d3d12.cpp)
  list(APPEND DEFS QB_HAVE_D3D)
  list(APPEND LIBS d3d9 d3d11 d3d12 dxgi dxguid)  # d3d10_1.dll is loaded at run time
endif()

add_executable(qorral-bench ${SOURCES})
target_include_directories(qorral-bench PRIVATE src ${GEN})
target_compile_definitions(qorral-bench PRIVATE ${DEFS})
target_link_libraries(qorral-bench PRIVATE ${LIBS})

if(MSVC)
  target_compile_options(qorral-bench PRIVATE /W3)
else()
  target_compile_options(qorral-bench PRIVATE -Wall -Wextra -Wno-missing-field-initializers)
endif()

if(MINGW)
  # Single binary: no libstdc++/libgcc/winpthread DLLs next to it, only SDL2.dll.
  target_link_options(qorral-bench PRIVATE -static-libgcc -static-libstdc++ $<$<CONFIG:Release>:-s>
    -Wl,-Bstatic,--whole-archive -lwinpthread -Wl,--no-whole-archive,-Bdynamic)
endif()

if(WIN32 AND TARGET SDL2::SDL2)
  add_custom_command(TARGET qorral-bench POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_if_different $<TARGET_FILE:SDL2::SDL2> $<TARGET_FILE_DIR:qorral-bench>
    COMMENT "Copying SDL2 runtime next to qorral-bench")
endif()

install(TARGETS qorral-bench RUNTIME DESTINATION bin)
