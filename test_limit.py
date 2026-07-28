import requests
import json
import time

url = "https://opencode.ai/zen/v1/chat/completions"
api_key = "sk-HXmf0912RCx5CngD9zpebOHa41tfu4sep81i3nXPGmU3SGEl5Vw7RFN7sLxFvfT2"

# To hit ~250k tokens, we need about 250,000 words.
# This should trigger a 413 error if the limit is strictly 200k, OR reveal silent truncation.
huge_text = "apple orange banana grape " * 62_500 # 250k words, ~300k tokens

headers = {
    "Authorization": f"Bearer {api_key}",
    "Content-Type": "application/json"
}

data = {
    "model": "mimo-v2.5-free",
    "messages": [
        {"role": "user", "content": f"Here is a huge block of text. {huge_text}\n\nPlease reply with 'received'."}
    ]
}

print(f"Sending request with {len(huge_text)} characters...")
start_time = time.time()
response = requests.post(url, headers=headers, json=data)
end_time = time.time()

print(f"Time taken: {end_time - start_time:.2f} seconds")
print(f"Status Code: {response.status_code}")
try:
    resp_json = response.json()
    print(f"Response JSON: {json.dumps(resp_json, indent=2)}")
except Exception as e:
    print(f"Response text: {response.text}")
