/*
	This file is part of solidity.

	solidity is free software: you can redistribute it and/or modify
	it under the terms of the GNU General Public License as published by
	the Free Software Foundation, either version 3 of the License, or
	(at your option) any later version.

	solidity is distributed in the hope that it will be useful,
	but WITHOUT ANY WARRANTY; without even the implied warranty of
	MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
	GNU General Public License for more details.

	You should have received a copy of the GNU General Public License
	along with solidity.  If not, see <http://www.gnu.org/licenses/>.
*/
/** @file JSON.h
 * @date 2016
 *
 * JSON related helpers
 */

#pragma once

#include <json/json.h>

namespace dev
{

/// Serialise the JSON object (@a _input) with indentation
inline std::string jsonPrettyPrint(Json::Value const& _input)
{
	return Json::StyledWriter().write(_input);
}

/// Serialise the JSON object (@a _input) without indentation
inline std::string jsonCompactPrint(Json::Value const& _input)
{
	Json::FastWriter writer;
	writer.omitEndingLineFeed();
	return writer.write(_input);
}

}
