from datetime import datetime
from flask import Flask
from flask_apscheduler import APScheduler
import json
import os
import requests
import time


app = Flask(__name__)
scheduler = APScheduler()
scheduler.init_app(app)
scheduler.start()

weather = {}
ecobee_home = {}

TOKEN_FILE = 'ecobee_tokens.json'


def _load_tokens():
    if os.path.exists(TOKEN_FILE):
        with open(TOKEN_FILE) as f:
            return json.load(f)
    return {}


def _save_tokens(tokens):
    with open(TOKEN_FILE, 'w') as f:
        json.dump(tokens, f)


def _get_access_token():
    """Return a valid access token, refreshing if expired. Returns None if not yet authorized."""
    tokens = _load_tokens()
    if not tokens or 'access_token' not in tokens:
        return None
    if time.time() >= tokens.get('expires_at', 0):
        r = requests.post(
            'https://api.ecobee.com/token',
            params={
                'grant_type': 'refresh_token',
                'refresh_token': tokens['refresh_token'],
                'client_id': os.getenv('ECOBEE_API_KEY'),
            }
        )
        if r.status_code != 200:
            print('error refreshing ecobee token - status code: ', r.status_code, ' body: ', r.text)
            return None
        tokens = r.json()
        tokens['expires_at'] = time.time() + tokens['expires_in'] - 60
        _save_tokens(tokens)
    return tokens['access_token']


# interval example
@scheduler.task('interval', id='update weather', minutes=5)
def update_weather():
    print('Updating weather')
    payload = {
        'lat': os.getenv('LAT'),
        'lon': os.getenv('LON'),
        'appid': os.getenv('APP_ID'),
        'units': os.getenv('UNITS', default='imperial')}
    r = requests.get("https://api.openweathermap.org/data/3.0/onecall?", params=payload)
    if r.status_code != 200:
        print('error updating weather - status code: ', r.status_code)
    else:
        global weather
        weather = r.text
        print('Weather updated')


@scheduler.task('interval', id='update ecobee home', minutes=3)
def update_ecobee_home():
    print('Updating ecobee home')
    access_token = _get_access_token()
    if not access_token:
        print('No ecobee access token - call /ecobee_authorize to set up authentication')
        return
    selection = json.dumps({
        "selection": {
            "selectionType": "registered",
            "selectionMatch": "",
            "includeRuntime": True,
            "includeSettings": True,
            "includeWeather": True,
            "includeSensors": True,
        }
    })
    headers = {'Authorization': f'Bearer {access_token}'}
    r = requests.get(
        "https://api.ecobee.com/1/thermostat",
        headers=headers,
        params={"json": selection},
    )
    if r.status_code != 200:
        print('error updating ecobee home - status code: ', r.status_code, ' body: ', r.text)
    else:
        global ecobee_home
        thermostat = r.json()['thermostatList'][0]
        runtime = thermostat['runtime']
        forecast = thermostat['weather']['forecasts'][0]

        sensors = []
        for sensor in thermostat.get('remoteSensors', []):
            s = {'name': sensor['name'], 'type': sensor['type']}
            for cap in sensor.get('capability', []):
                if cap['type'] == 'temperature' and cap['value'] != 'unknown':
                    s['temperature'] = int(cap['value']) / 10
                elif cap['type'] == 'occupancy':
                    s['occupancy'] = cap['value'] == 'true'
            sensors.append(s)

        ecobee_home = {
            'name': thermostat['name'],
            'connected': runtime['connected'],
            'hvac_mode': thermostat['settings']['hvacMode'],
            'equipment_status': thermostat.get('equipmentStatus', ''),
            'indoor': {
                'temperature': runtime['actualTemperature'] / 10,
                'raw_temperature': runtime['rawTemperature'] / 10,
                'humidity': runtime['actualHumidity'],
            },
            'outdoor': {
                'temperature': forecast['temperature'] / 10,
                'humidity': forecast['relativeHumidity'],
                'condition': forecast['condition'],
                'dewpoint': forecast['dewpoint'] / 10,
                'wind_speed': forecast['windSpeed'],
                'wind_direction': forecast['windDirection'],
            },
            'setpoints': {
                'heat': runtime['desiredHeat'] / 10,
                'cool': runtime['desiredCool'] / 10,
                'fan_mode': runtime['desiredFanMode'],
                'humidity': runtime['desiredHumidity'],
            },
            'sensors': sensors,
        }
        print('Ecobee home updated')


@app.route('/')
def show_weather():
    return weather

@app.route('/owm_oneshot')
def show_owm_oneshot():
    return weather

@app.route('/ecobee_authorize')
def ecobee_authorize():
    r = requests.get(
        'https://api.ecobee.com/authorize',
        params={
            'response_type': 'ecobeePin',
            'client_id': os.getenv('ECOBEE_API_KEY'),
            'scope': 'smartRead',
        }
    )
    if r.status_code != 200:
        return json.dumps({'error': r.text}), r.status_code
    data = r.json()
    _save_tokens({'pending_code': data['code']})
    return json.dumps({
        'pin': data['ecobeePin'],
        'expires_in_seconds': data['expires_in'],
        'instructions': 'Go to ecobee.com > My Apps and enter this PIN, then call /ecobee_complete_auth',
    })

@app.route('/ecobee_complete_auth')
def ecobee_complete_auth():
    code = _load_tokens().get('pending_code')
    if not code:
        return json.dumps({'error': 'No pending authorization. Call /ecobee_authorize first.'}), 400
    r = requests.post(
        'https://api.ecobee.com/token',
        params={
            'grant_type': 'ecobeePin',
            'code': code,
            'client_id': os.getenv('ECOBEE_API_KEY'),
        }
    )
    if r.status_code != 200:
        return json.dumps({'error': r.text}), r.status_code
    tokens = r.json()
    tokens['expires_at'] = time.time() + tokens['expires_in'] - 60
    _save_tokens(tokens)
    return json.dumps({'status': 'authorized'})

@app.route('/ecobee_home')
def show_ecobee_home():
    return json.dumps(ecobee_home)

@app.route('/epoch')
def show_epoch():
    return json.dumps(int(time.time()))


if __name__ == '__main__':
    update_weather()
    update_ecobee_home()
    app.run(host="0.0.0.0")
