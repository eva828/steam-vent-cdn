
import os
import zipfile
import subprocess
import shutil
import requests
import sys

STEAMCMD_URL = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd.zip"
DIR = "steamcmd_tool"
EXE = os.path.join(DIR, "steamcmd.exe")
APP_ID = "1281930"
WORKSHOP_ID = "2824688072"

def main():
    if os.path.exists(DIR):
        print(f"Directory {DIR} exists. Skipping download.")
    else:
        print(f"Downloading steamcmd from {STEAMCMD_URL}...")
        r = requests.get(STEAMCMD_URL)
        r.raise_for_status()
        with open("steamcmd.zip", "wb") as f:
            f.write(r.content)
        
        print("Extracting...")
        with zipfile.ZipFile("steamcmd.zip", "r") as z:
            z.extractall(DIR)
        os.remove("steamcmd.zip")
    
    # Run 1: Update
    print("Running steamcmd (1/2) to update...")
    update_cmd = [EXE, "+quit"]
    try:
        subprocess.run(update_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=300)
    except Exception:
        pass # Ignore errors during update
        
    # Run 2: Download
    print("Running steamcmd (2/2) to attempt anonymous download...")
    
    cmd = [
        EXE,
        "+login", "anonymous",
        "+workshop_download_item", APP_ID, WORKSHOP_ID,
        "+quit"
    ]
    
    print(f"Executing: {' '.join(cmd)}")
    
    with open("steamcmd_output.txt", "w", encoding="utf-8") as outfile:
        try:
            # Use errors='replace' to avoid crashing on non-utf8 output from the process
            # conflicting with our file writing? No, process.stdout is bytes if we don't specify text=True
            # Let's use bytes and decode manually to be safe
            process = subprocess.run(
                cmd, 
                stdout=subprocess.PIPE, 
                stderr=subprocess.STDOUT, 
                timeout=300
            )
            
            output = process.stdout.decode('utf-8', errors='replace')
            outfile.write(output)
            
            print("--- Output excerpt ---")
            print(output[-1000:] if len(output) > 1000 else output)
            print("--- End Output ---")
            
            if "Success. Downloaded item" in output:
                print("SUCCESS: steamcmd successfully downloaded the item anonymously.")
            elif "ERROR! Download item" in output:
                print("FAILURE: steamcmd failed to download the item.")
            else:
                print("UNKNOWN: Could not determine success/failure from output.")
                
        except subprocess.TimeoutExpired:
            outfile.write("\nTIMEOUT: steamcmd took too long.")
            print("TIMEOUT: steamcmd took too long.")
        except Exception as e:
            outfile.write(f"\nERROR: {e}")
            print(f"ERROR: {e}")

if __name__ == "__main__":
    main()
