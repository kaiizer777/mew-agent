import requests
import json
import base64

url = "https://opencode.ai/zen/v1/chat/completions"
api_key = "sk-HXmf0912RCx5CngD9zpebOHa41tfu4sep81i3nXPGmU3SGEl5Vw7RFN7sLxFvfT2"

# 1x1 black png
b64_img = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII="

headers = {
    "Authorization": f"Bearer {api_key}",
    "Content-Type": "application/json"
}

data = {
    "model": "mimo-v2.5-free",
    "messages": [
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "What is in this image?"},
                {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{b64_img}"}}
            ]
        }
    ]
}

response = requests.post(url, headers=headers, json=data)

print(f"Status Code: {response.status_code}")
try:
    print(f"Response JSON:\n{json.dumps(response.json(), indent=2)}")
except Exception:
    print(f"Response text:\n{response.text}")
