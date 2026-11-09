// Test only: makes a non-root dropbear authenticate "root" with password
// "alpine" and run commands as the invoking user, as the ramdisk's does as root.
#define _GNU_SOURCE
#include <pwd.h>
#include <string.h>
#include <unistd.h>
#include <stdlib.h>
static struct passwd pw;
struct passwd *getpwnam(const char *name) {
    if (strcmp(name, "root")) return NULL;
    pw.pw_name = "root"; pw.pw_passwd = "stored"; pw.pw_uid = getuid(); pw.pw_gid = getgid();
    pw.pw_gecos = ""; pw.pw_dir = getenv("SHIM_HOME"); pw.pw_shell = "/bin/sh";
    return &pw;
}
char *crypt(const char *key, const char *salt) { (void)salt; return strcmp(key, "alpine") ? "wrong" : "stored"; }
void *getspnam(const char *n) { (void)n; return NULL; }
// The ramdisk runs as root; here a chown to root is accepted and not applied.
int chown(const char *p, unsigned u, unsigned g) { (void)p; (void)u; (void)g; return 0; }
int fchown(int f, unsigned u, unsigned g) { (void)f; (void)u; (void)g; return 0; }
int lchown(const char *p, unsigned u, unsigned g) { (void)p; (void)u; (void)g; return 0; }
