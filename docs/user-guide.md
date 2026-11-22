# Backer user guide

## Set up a backup
1. Open Backer and choose **New job**.
2. Pick the folders to back up.
3. Pick the NAS. You can connect directly (SMB, NFS or SFTP; Backer lists the shares it finds) or choose a folder on a
   share your computer has already mounted. Backer puts a small `.backer-destination` marker file there so it can tell
   the share really is connected.
4. Choose a schedule and any exclude patterns (`*.tmp`, `node_modules/`) and review.

Each source folder lands in a folder of the same name under the NAS folder you chose.

## Deleted files
Backer never deletes anything on the NAS by itself. A file you delete on your computer stays on the NAS and is listed
under **Deleted files**. Select files and confirm to remove them from the NAS.

## Restore
Open the job, choose **Browse backup**, tick files or folders and restore them to the original place or another
folder. Backer lists files that already exist and replaces them only if you say so. Restore works after the trial ends.

## Trial and license
The trial has every feature for 14 days. After that backups stop; history, Verify and restore keep working. Enter your
key under Settings. A key covers every 1.x version.

## If something goes wrong
- **"NAS not reachable"**: check the NAS is on and the share is mounted or the address is right. Backer retries and
  then sends a notification.
- **Nothing runs on schedule**: make sure the Backer service is running (menu bar or system tray icon).
- **Contact support** at info@turtlebox.studio and attach the service log:
  - Mac: `~/Library/Application Support/Backer/agent.log`
  - Windows: `%LOCALAPPDATA%\Backer\agent.log`
  The log holds service events only (start-up, errors), not your file names.

## Known limits
- SFTP key-file login is not available on Windows (password login works).
- On Windows, an AI assistant is approved as one shared "unverified" program, because Windows cannot tell which
  program is on a local port.
