
import logging
import requests
import os
from steam.client import SteamClient
from steam.client.cdn import CDNClient

# Configuration
APP_ID = 1281930  # tModLoader
PUBLISHED_FILE_ID = 2824688072 # Calamity Mod (example)

# Setup logging
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logging.getLogger("steam.client.cdn").setLevel(logging.DEBUG)
logging.getLogger("steam.client").setLevel(logging.INFO) # Keep client info to reduce noise
LOG = logging.getLogger("TestDownload")

def main():
    LOG.info("Initializing Steam Client...")
    client = SteamClient()
    
    LOG.info("Logging in anonymously...")
    client.anonymous_login()
    LOG.info(f"Logged in successfully. SteamID: {client.steam_id}")
    
    # Load API Key
    api_key = None
    if os.path.exists(".env"):
        try:
            with open(".env", "r") as f:
                for line in f:
                    if line.strip().startswith("STEAM_API_USER_KEY="):
                        api_key = line.strip().split("=", 1)[1]
                        LOG.info("Loaded STEAM_API_USER_KEY from .env")
                        break
        except Exception as e:
            LOG.warning(f"Failed to read .env: {e}")

    # 1. Get Workshop Item Details via WebAPI
    LOG.info(f"Fetching details for Workshop Item {PUBLISHED_FILE_ID}...")
    url = "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/"
    data = {'itemcount': 1, 'publishedfileids[0]': PUBLISHED_FILE_ID}
    if api_key:
        data['key'] = api_key
    
    try:
        resp = requests.post(url, data=data)
        resp.raise_for_status()
        json_resp = resp.json()
        details = json_resp['response']['publishedfiledetails'][0]
    except Exception as e:
        LOG.error(f"Failed to get file details: {e}")
        return

    if details.get('result') != 1:
        LOG.error(f"API Error Result: {details.get('result')}")
        return

    # Check for direct download URL
    file_url = details.get('file_url')
    if file_url:
        LOG.info(f"Found direct download URL: {file_url}")
        LOG.info("Attempting direct download...")
        try:
            r = requests.get(file_url, stream=True)
            r.raise_for_status()
            with open("downloaded_file_direct.tmod", 'wb') as f:
                for chunk in r.iter_content(chunk_size=8192):
                    f.write(chunk)
            LOG.info("Successfully downloaded via direct URL!")
            return
        except Exception as e:
            LOG.error(f"Direct download failed: {e}")
            LOG.info("Falling back to SteamPipe CDN...")
    else:
        LOG.info("No direct download URL found. Proceeding with SteamPipe CDN...")

    hcontent_file = details.get('hcontent_file')
    if not hcontent_file:
        LOG.error("No manifest ID (hcontent_file) found for this item.")
        return

    manifest_gid = int(hcontent_file)
    consumer_app_id = int(details.get('consumer_app_id', APP_ID))
    
    LOG.info(f"Target: AppID={consumer_app_id}, DepotID={consumer_app_id}, ManifestGID={manifest_gid}")
    
    # 2. Initialize CDN Client
    LOG.info("Initializing CDN Client...")
    cdn = CDNClient(client)
    
    # 3. Download Manifest
    LOG.info("Fetching Manifest...")
    
    # depot_id for workshop items is the consumer_app_id
    depot_id = consumer_app_id
    
    LOG.info(f"Calling GetManifestRequestCode(app_id={consumer_app_id}, depot_id={depot_id}, manifest_gid={manifest_gid})")
    # code = cdn.get_manifest_request_code(consumer_app_id, depot_id, manifest_gid)
    # LOG.info(f"GetManifestRequestCode returned: {code}")
    
    # Test GetCDNAuthToken
    if cdn.servers:
        server = cdn.servers[0] # Pick first server
        host = server.host
        LOG.info(f"Attempting GetCDNAuthToken for depot={depot_id}, host={host}")
        
        # Use unified messages directly
        req_data = {
            'depot_id': depot_id,
            'host_name': host,
            'app_id': consumer_app_id # Try adding app_id too
        }
        try:
            token_resp = client.unified_messages.get("ContentServerDirectory.GetCDNAuthToken#1", **req_data)
            LOG.info(f"GetCDNAuthToken response: {token_resp}")
        except Exception as e:
            LOG.error(f"GetCDNAuthToken failed: {e}")
    else:
            LOG.warning("No CDN servers found to test token.")

    # This triggers:
    # 1. GetServersForSteamPipe
    # 2. GetDepotDecryptionKey
    # 3. GetManifestRequestCode
    # 4. Download and Decrypt Manifest
    manifest = cdn.get_manifest(consumer_app_id, depot_id, manifest_gid)
    
    LOG.info(f"Manifest fetched successfully!")
    LOG.info(f"Files in manifest: {len(manifest.payload.mappings)}")
    
    for mapping in manifest.payload.mappings[:3]: # Show first 3 files
        filename = mapping.filename
        size = mapping.size
        LOG.info(f" - {filename} ({size} bytes)")
        
    LOG.info("Attempting to download the first file to confirm connectivity...")
    
    if manifest.payload.mappings:
        first_file = manifest.payload.mappings[0]
        filename = first_file.filename
        sanitized_name = os.path.basename(filename)
        
        LOG.info(f"Downloading {filename} as {sanitized_name}...")
        
        # cdn.iter_files returns a generator yielding (filename, file_handle)
        # Find our file
        found = False
        for f_name, f_handle in cdn.iter_files(consumer_app_id, depot_id, manifest):
            if f_name == filename:
                found = True
                with open(sanitized_name, 'wb') as out_f:
                    for chunk in iter(lambda: f_handle.read(4096), b''):
                        out_f.write(chunk)
                LOG.info(f"Successfully downloaded {sanitized_name}")
                break
        
        if not found:
            LOG.error(f"File {filename} not found in iterator.")
                
    except Exception as e:
        LOG.error(f"CDN content fetch failed: {e}")
        import traceback
        traceback.print_exc()

if __name__ == "__main__":
    main()
