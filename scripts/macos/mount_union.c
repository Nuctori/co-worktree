/*
 * mount_union — replacement for the macOS union mount helper that Apple
 * removed from /Library/Filesystems/union.fs (macOS 15+; also missing on
 * current macos-14 images).
 *
 * Installed to:
 *   /Library/Filesystems/union.fs/Contents/Resources/mount_union
 *
 * mount(8) invokes it as:  mount_union [-o options] <special> <node>
 * where <special> is the upper directory and <node> is the mountpoint whose
 * previous content becomes the lower layer (classic BSD union semantics).
 * The kernel's union vfs is still present in xnu; only the userspace helper
 * was dropped.
 */
#include <sys/mount.h>
#include <stdio.h>
#include <string.h>

struct union_args {
    int mntflags;
    char *target; /* upper layer */
    char *vnode;  /* lower layer (mountpoint content) */
};

int main(int argc, char **argv) {
    const char *special = NULL;
    const char *node = NULL;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "-o") == 0 && i + 1 < argc) {
            i++; /* skip options; nobrowse etc. are cosmetic for cowt */
            continue;
        }
        if (argv[i][0] == '-') {
            continue;
        }
        if (special == NULL) {
            special = argv[i];
        } else if (node == NULL) {
            node = argv[i];
        }
    }
    if (special == NULL || node == NULL) {
        fprintf(stderr, "usage: mount_union [-o options] upper mountpoint\n");
        return 64;
    }

    struct union_args ua;
    memset(&ua, 0, sizeof(ua));
    ua.mntflags = 0;
    ua.target = (char *)special;
    ua.vnode = (char *)node;

    if (mount("union", node, MNT_UNION, &ua) < 0) {
        perror("mount_union");
        return 1;
    }
    return 0;
}
