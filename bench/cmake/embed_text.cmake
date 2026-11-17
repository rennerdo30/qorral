# Turns a text file (generated GLSL or HLSL) into a C++ header with a raw string literal.
# Usage: cmake -DIN=x.glsl -DOUT=x.h -DVAR=name -P embed_text.cmake
file(READ "${IN}" text)
string(FIND "${text}" ")QBSRC\"" bad)
if(NOT bad EQUAL -1)
  message(FATAL_ERROR "${IN} contains the raw-string delimiter")
endif()
file(WRITE "${OUT}" "// Generated from ${IN}; do not edit.\n#pragma once\nstatic const char ${VAR}[] = R\"QBSRC(${text})QBSRC\";\n")
