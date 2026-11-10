/* What the u_gralloc sources link against in a driver, for gralloc_test.c. */
#include <stdarg.h>
#include <stdio.h>
#include <stddef.h>

#include "util/log.h"

struct u_gralloc;

void
mesa_log(enum mesa_log_level level, const char *tag, const char *format, ...)
{
   va_list va;

   va_start(va, format);
   vfprintf(stderr, format, va);
   va_end(va);
   fputc('\n', stderr);
}
