import requests

url = 'https://www.alphavantage.co/query?function=TIME_SERIES_DAILY&symbol=AMAT&apikey=VGQAZO40GZSSXXU3'
r = requests.get(url)
data = r.json()

print(data)