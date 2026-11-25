// Small Objective-C layer for the Mac App Store edition: things the sandbox only allows through system frameworks.
// Plain C functions so Rust can call them without any binding crates. Built only with the `mas` feature (see build.rs).
#import <Foundation/Foundation.h>
#import <ServiceManagement/ServiceManagement.h>
#import <UserNotifications/UserNotifications.h>
#include <stdlib.h>
#include <string.h>

void backer_free(unsigned char *p) { free(p); }

// ---- Security-scoped bookmarks: how a sandboxed app keeps access to a folder the user picked ----

// Bookmark bytes for a folder the user has just chosen (malloc'd; free with backer_free), or NULL.
unsigned char *backer_bookmark_create(const char *path, size_t *len) {
    @autoreleasepool {
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path] isDirectory:YES];
        NSError *err = nil;
        NSData *d = [url bookmarkDataWithOptions:NSURLBookmarkCreationWithSecurityScope
                  includingResourceValuesForKeys:nil
                                   relativeToURL:nil
                                           error:&err];
        if (!d) return NULL;
        unsigned char *out = malloc(d.length);
        if (!out) return NULL;
        memcpy(out, d.bytes, d.length);
        *len = d.length;
        return out;
    }
}

// Resolves a bookmark and starts access to the folder for the rest of the process.
// Returns 0 when that failed, 1 when it worked, 2 when it worked but the bookmark is stale and should be remade.
int backer_bookmark_open(const unsigned char *bytes, size_t len) {
    @autoreleasepool {
        NSData *d = [NSData dataWithBytes:bytes length:len];
        BOOL stale = NO;
        NSError *err = nil;
        NSURL *url = [NSURL URLByResolvingBookmarkData:d
                                               options:NSURLBookmarkResolutionWithSecurityScope
                                         relativeToURL:nil
                                   bookmarkDataIsStale:&stale
                                                 error:&err];
        if (!url) return 0;
        if (![url startAccessingSecurityScopedResource]) return 0;
        return stale ? 2 : 1;
    }
}

// ---- Launch at login (macOS 13+) ----

// 0 off, 1 on, 2 waiting for the user to allow it in System Settings, Login Items.
int backer_login_item_status(void) {
    if (@available(macOS 13.0, *)) {
        switch ([SMAppService mainAppService].status) {
            case SMAppServiceStatusEnabled: return 1;
            case SMAppServiceStatusRequiresApproval: return 2;
            default: return 0;
        }
    }
    return 0;
}

int backer_login_item_set(int on) {
    if (@available(macOS 13.0, *)) {
        NSError *err = nil;
        SMAppService *s = [SMAppService mainAppService];
        BOOL ok = on ? [s registerAndReturnError:&err] : [s unregisterAndReturnError:&err];
        return ok ? 1 : 0;
    }
    return 0;
}

// True when macOS started this app because the user logged in (so it should stay in the menu bar, window hidden).
int backer_launched_as_login_item(void) {
    NSAppleEventDescriptor *ev = [[NSAppleEventManager sharedAppleEventManager] currentAppleEvent];
    if (!ev || ev.eventID != kAEOpenApplication) return 0;
    NSAppleEventDescriptor *prop = [ev paramDescriptorForKeyword:keyAEPropData];
    return (prop && prop.enumCodeValue == keyAELaunchedAsLogInItem) ? 1 : 0;
}

// ---- Notifications ----

void backer_notify(const char *title, const char *body) {
    @autoreleasepool {
        NSString *t = [NSString stringWithUTF8String:title];
        NSString *b = [NSString stringWithUTF8String:body];
        UNUserNotificationCenter *c = [UNUserNotificationCenter currentNotificationCenter];
        [c requestAuthorizationWithOptions:(UNAuthorizationOptionAlert | UNAuthorizationOptionSound)
                         completionHandler:^(BOOL granted, NSError *error) {
            if (!granted) return;
            UNMutableNotificationContent *m = [UNMutableNotificationContent new];
            m.title = t;
            m.body = b;
            UNNotificationRequest *r = [UNNotificationRequest requestWithIdentifier:[[NSUUID UUID] UUIDString] content:m trigger:nil];
            [c addNotificationRequest:r withCompletionHandler:nil];
        }];
    }
}
